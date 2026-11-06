//! Bounded NIP-77 sessions over the relay's locally readable event snapshot.
use super::imp::NostrRelay;
use ::negentropy::{Id, Negentropy, NegentropyStorageVector};
use nostr::{Event, Filter, JsonUtil, RelayMessage, SubscriptionId};
use std::borrow::Cow;
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub(super) const MAX_RECORDS: usize = 1024;
pub(super) const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;
const MAX_HEX_BYTES: usize = 65536;
const MAX_SESSIONS: usize = 64;
const MAX_CLIENT_SESSIONS: usize = 2;
const SESSION_TTL: Duration = Duration::from_secs(30);

struct Session {
    generation: u64,
    touched: Instant,
    protocol: Option<Negentropy<'static, NegentropyStorageVector>>,
    rounds: u32,
    bytes: usize,
}

#[derive(Default)]
pub(super) struct Sessions {
    entries: HashMap<(u64, SubscriptionId), Session>,
    generation: u64,
}

impl Sessions {
    fn expire(&mut self) {
        self.entries
            .retain(|_, s| s.touched.elapsed() < SESSION_TTL);
    }

    pub(super) fn close(&mut self, client: u64, id: &SubscriptionId) {
        self.entries.remove(&(client, id.clone()));
    }

    pub(super) fn disconnect(&mut self, client: u64) {
        self.entries.retain(|(owner, _), _| *owner != client);
    }

    fn reserve(&mut self, client: u64, id: &SubscriptionId) -> Result<u64, &'static str> {
        self.expire();
        // NEG and REQ identifiers have independent namespaces.
        self.close(client, id);
        if self.entries.len() >= MAX_SESSIONS
            || self
                .entries
                .keys()
                .filter(|(owner, _)| *owner == client)
                .count()
                >= MAX_CLIENT_SESSIONS
        {
            return Err("blocked: too many reconciliation sessions");
        }
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.entries.insert(
            (client, id.clone()),
            Session {
                generation,
                touched: Instant::now(),
                protocol: None,
                rounds: 0,
                bytes: 0,
            },
        );
        Ok(generation)
    }

    fn is_current(&self, client: u64, id: &SubscriptionId, generation: u64) -> bool {
        self.entries
            .get(&(client, id.clone()))
            .is_some_and(|s| s.generation == generation)
    }

    fn install(
        &mut self,
        client: u64,
        id: &SubscriptionId,
        generation: u64,
        events: &[Event],
    ) -> Result<bool, &'static str> {
        self.expire();
        if !self.is_current(client, id, generation) {
            return Ok(false);
        }
        if events.len() > MAX_RECORDS {
            return Err("blocked: reconciliation set too large");
        }
        let mut storage = NegentropyStorageVector::new();
        for event in events {
            if event.created_at.as_secs() == u64::MAX {
                return Err("error: invalid local reconciliation timestamp");
            }
            storage
                .insert(
                    event.created_at.as_secs(),
                    Id::from_byte_array(event.id.to_bytes()),
                )
                .map_err(|_| "error: invalid local reconciliation record")?;
        }
        storage
            .seal()
            .map_err(|_| "error: invalid local reconciliation set")?;
        let protocol = Negentropy::owned(storage, 16384)
            .map_err(|_| "error: cannot initialize reconciliation")?;
        self.entries
            .get_mut(&(client, id.clone()))
            .expect("current reservation")
            .protocol = Some(protocol);
        Ok(true)
    }

    fn respond(
        &mut self,
        client: u64,
        id: &SubscriptionId,
        message: &str,
    ) -> Result<String, &'static str> {
        self.expire();
        let result = (|| {
            let bytes = decode_message(message)?;
            let session = self
                .entries
                .get_mut(&(client, id.clone()))
                .ok_or("closed: unknown or expired reconciliation")?;
            if session.rounds >= 32 || session.bytes.saturating_add(message.len()) > 1024 * 1024 {
                return Err("blocked: reconciliation work limit");
            }
            session.rounds += 1;
            session.bytes += message.len();
            session.touched = Instant::now();
            let response = session
                .protocol
                .as_mut()
                .ok_or("closed: reconciliation not ready")?
                .reconcile(&bytes)
                .map_err(|_| "error: invalid reconciliation message")?;
            Ok(hex::encode(response))
        })();
        if result.is_err() {
            self.close(client, id);
        }
        result
    }
}

fn decode_message(message: &str) -> Result<Vec<u8>, &'static str> {
    if message.is_empty() || message.len() > MAX_HEX_BYTES {
        return Err("blocked: reconciliation message size");
    }
    hex::decode(message).map_err(|_| "error: invalid reconciliation encoding")
}

impl NostrRelay {
    pub(super) async fn handle_neg_open(
        &self,
        client: u64,
        id: SubscriptionId,
        filter: Filter,
        id_size: Option<u8>,
        message: String,
    ) {
        // A replacement OPEN closes the old session even if the new request is rejected.
        self.negentropy.lock().await.close(client, &id);
        let error = if id.to_string().len() > 64 || filter.as_json().len() > MAX_HEX_BYTES {
            Some("blocked: reconciliation filter size")
        } else if id_size.is_some() || filter.limit.is_some() || filter.search.is_some() {
            Some("blocked: legacy, limited and search reconciliation filters are unsupported")
        } else if let Err(error) = decode_message(&message) {
            Some(error)
        } else if !self.allow_req(client).await {
            Some("rate-limited: reconciliation request limit")
        } else {
            None
        };
        if let Some(error) = error {
            self.send_neg_error(client, id, error).await;
            return;
        }
        let reservation = self.negentropy.lock().await.reserve(client, &id);
        let generation = match reservation {
            Ok(generation) => generation,
            Err(error) => {
                self.send_neg_error(client, id, error).await;
                return;
            }
        };
        let snapshot = self.negentropy_snapshot(filter).await;
        let mut sessions = self.negentropy.lock().await;
        if !sessions.is_current(client, &id, generation) {
            return;
        }
        let result = match snapshot {
            Ok(events) => match sessions.install(client, &id, generation, &events) {
                Ok(true) => sessions.respond(client, &id, &message),
                Ok(false) => return,
                Err(error) => Err(error),
            },
            Err(error) => {
                tracing::warn!("local Negentropy snapshot unavailable: {error}");
                Err("blocked: local reconciliation snapshot unavailable or over budget")
            }
        };
        if result.is_err() {
            sessions.close(client, &id);
        }
        drop(sessions);
        self.send_neg_result(client, id, result).await;
    }

    pub(super) async fn handle_neg_message(
        &self,
        client: u64,
        id: SubscriptionId,
        message: String,
    ) {
        let result = self.negentropy.lock().await.respond(client, &id, &message);
        self.send_neg_result(client, id, result).await;
    }

    async fn send_neg_result(
        &self,
        client: u64,
        id: SubscriptionId,
        result: Result<String, &'static str>,
    ) {
        match result {
            Ok(message) => {
                self.send_to_client(
                    client,
                    RelayMessage::NegMsg {
                        subscription_id: Cow::Owned(id),
                        message: message.into(),
                    },
                )
                .await
            }
            Err(error) => self.send_neg_error(client, id, error).await,
        }
    }

    async fn send_neg_error(&self, client: u64, id: SubscriptionId, error: &'static str) {
        self.send_to_client(
            client,
            RelayMessage::NegErr {
                subscription_id: Cow::Owned(id),
                message: error.into(),
            },
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::ClientMessage;
    use nostr::{EventBuilder, Keys, Kind, Timestamp};
    use tokio::sync::mpsc;

    async fn response(rx: &mut mpsc::UnboundedReceiver<String>) -> RelayMessage<'static> {
        let text = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        RelayMessage::from_json(text).unwrap()
    }

    async fn reconcile_relay(
        relay: &NostrRelay,
        rx: &mut mpsc::UnboundedReceiver<String>,
        id: &SubscriptionId,
        filter: Filter,
        local: &[Event],
    ) -> Vec<Id> {
        let mut client = peer(local);
        relay
            .handle_client_message(
                7,
                ClientMessage::NegOpen {
                    subscription_id: Cow::Owned(id.clone()),
                    filter: Cow::Owned(filter),
                    id_size: None,
                    initial_message: hex::encode(client.initiate().unwrap()).into(),
                },
            )
            .await;
        let mut missing = Vec::new();
        for _ in 0..32 {
            let RelayMessage::NegMsg {
                subscription_id,
                message,
            } = response(rx).await
            else {
                panic!("expected NEG-MSG");
            };
            assert_eq!(subscription_id.as_ref(), id);
            match client
                .reconcile_with_ids(
                    &hex::decode(message.as_ref()).unwrap(),
                    &mut Vec::new(),
                    &mut missing,
                )
                .unwrap()
            {
                Some(message) => {
                    relay
                        .handle_client_message(
                            7,
                            ClientMessage::NegMsg {
                                subscription_id: Cow::Owned(id.clone()),
                                message: hex::encode(message).into(),
                            },
                        )
                        .await
                }
                None => return missing,
            }
        }
        panic!("reconciliation did not finish");
    }

    #[tokio::test]
    async fn negentropy_serves_full_local_sets_above_req_limit_and_retains_live_subscription() {
        let directory = tempfile::tempdir().unwrap();
        let relay = NostrRelay::new_read_only(
            directory.path().to_path_buf(),
            super::super::NostrRelayConfig::default(),
        );
        let records = (0..300)
            .map(|i| {
                let keys = Keys::parse(format!("{:064x}", i / 2 + 1)).unwrap();
                EventBuilder::new(
                    if i % 2 == 0 {
                        Kind::ContactList
                    } else {
                        Kind::MuteList
                    },
                    "",
                )
                .custom_created_at(Timestamp::from_secs(1700000000))
                .sign_with_keys(&keys)
                .unwrap()
            })
            .collect::<Vec<_>>();
        for event in &records {
            relay
                .ingest_trusted_event_silent(event.clone())
                .await
                .unwrap();
        }
        let (tx, mut rx) = mpsc::unbounded_channel();
        relay.register_client(7, tx, None).await;
        let id = SubscriptionId::new("shared-with-req");
        let filter = Filter::new().kinds([Kind::ContactList, Kind::MuteList]);
        let missing = reconcile_relay(&relay, &mut rx, &id, filter.clone(), &[]).await;
        assert_eq!(
            missing.len(),
            records.len(),
            "ordinary REQ cap must not silently truncate NEG"
        );
        for chunk in records.chunks(64) {
            relay
                .handle_client_message(
                    7,
                    ClientMessage::req(
                        id.clone(),
                        vec![Filter::new().ids(chunk.iter().map(|e| e.id))],
                    ),
                )
                .await;
            let mut fetched = std::collections::HashSet::new();
            loop {
                match response(&mut rx).await {
                    RelayMessage::Event { event, .. } => {
                        assert!(event.verify().is_ok());
                        fetched.insert(event.id);
                    }
                    RelayMessage::EndOfStoredEvents(_) => break,
                    other => panic!("unexpected exact-ID response: {other:?}"),
                }
            }
            assert_eq!(fetched, chunk.iter().map(|e| e.id).collect());
        }
        assert!(
            reconcile_relay(&relay, &mut rx, &id, filter.clone(), &records)
                .await
                .is_empty()
        );
        relay
            .handle_client_message(7, ClientMessage::req(id.clone(), vec![filter]))
            .await;
        while !matches!(response(&mut rx).await, RelayMessage::EndOfStoredEvents(_)) {}
        relay
            .handle_client_message(
                7,
                ClientMessage::NegClose {
                    subscription_id: Cow::Owned(id.clone()),
                },
            )
            .await;
        let fresh = EventBuilder::new(Kind::ContactList, "")
            .sign_with_keys(&Keys::generate())
            .unwrap();
        relay.ingest_trusted_event(fresh.clone()).await.unwrap();
        match response(&mut rx).await {
            RelayMessage::Event {
                event,
                subscription_id,
            } => {
                assert_eq!(event.id, fresh.id);
                assert_eq!(subscription_id.as_ref(), &id);
            }
            other => panic!("NEG-CLOSE must preserve the REQ subscription: {other:?}"),
        }
        relay.unregister_client(7).await;
        assert!(relay.negentropy.lock().await.entries.is_empty());
    }

    #[tokio::test]
    async fn negentropy_reads_persisted_graph_bodies_through_bounded_backend() {
        let _guard = crate::socialgraph::test_lock().await;
        let directory = tempfile::tempdir().unwrap();
        let graph = crate::socialgraph::open_test_social_graph_store_with_mapsize(
            directory.path(),
            Some(128 * 1024 * 1024),
        )
        .unwrap();
        let keys = Keys::generate();
        let records = [Kind::ContactList, Kind::MuteList]
            .into_iter()
            .map(|kind| EventBuilder::new(kind, "").sign_with_keys(&keys).unwrap())
            .collect::<Vec<_>>();
        crate::socialgraph::ingest_parsed_events(graph.as_ref(), &records).unwrap();
        let backend: std::sync::Arc<dyn crate::socialgraph::SocialGraphBackend> = graph;
        let relay = NostrRelay::new(
            backend,
            directory.path().to_path_buf(),
            Default::default(),
            None,
            super::super::NostrRelayConfig {
                spambox_db_max_bytes: 0,
                ..Default::default()
            },
        )
        .unwrap();
        let filter = Filter::new()
            .author(keys.public_key())
            .kinds([Kind::ContactList, Kind::MuteList]);
        let actual = relay.negentropy_snapshot(filter).await.unwrap();
        assert_eq!(
            actual
                .iter()
                .map(|e| e.id)
                .collect::<std::collections::HashSet<_>>(),
            records.iter().map(|e| e.id).collect()
        );
    }

    #[tokio::test]
    async fn negentropy_corrupt_checkpoint_is_an_error_not_an_empty_set() {
        let directory = tempfile::tempdir().unwrap();
        let store =
            crate::storage::HashtreeStore::with_options(directory.path(), None, 128 * 1024 * 1024)
                .unwrap();
        let index = directory.path().join("nostr-index");
        std::fs::create_dir_all(&index).unwrap();
        let relay = NostrRelay::new_read_only(
            directory.path().to_path_buf(),
            super::super::NostrRelayConfig::default(),
        )
        .with_historical_nostr_index(store.store_arc(), directory.path().to_path_buf());
        let (tx, mut rx) = mpsc::unbounded_channel();
        relay.register_client(7, tx, None).await;
        for contents in ["corrupt-root\n", "", " \n"] {
            std::fs::write(index.join("latest-root.txt"), contents).unwrap();
            relay
                .handle_client_message(
                    7,
                    ClientMessage::NegOpen {
                        subscription_id: Cow::Owned(SubscriptionId::new("corrupt")),
                        filter: Cow::Owned(Filter::new()),
                        id_size: None,
                        initial_message: hex::encode(peer(&[]).initiate().unwrap()).into(),
                    },
                )
                .await;
            assert!(matches!(
                response(&mut rx).await,
                RelayMessage::NegErr { .. }
            ));
            assert!(relay.negentropy.lock().await.entries.is_empty());
        }
    }

    #[tokio::test]
    async fn negentropy_filters_authors_kinds_and_time_and_keeps_an_immutable_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let relay = NostrRelay::new_read_only(
            directory.path().to_path_buf(),
            super::super::NostrRelayConfig::default(),
        );
        let keys = Keys::generate();
        let records = (0..5)
            .map(|i| {
                EventBuilder::new(Kind::TextNote, format!("fixture {i}"))
                    .custom_created_at(Timestamp::from_secs(1700000000 + i))
                    .sign_with_keys(&keys)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        for event in &records {
            relay
                .ingest_trusted_event_silent(event.clone())
                .await
                .unwrap();
        }
        let filter = Filter::new()
            .author(keys.public_key())
            .kind(Kind::TextNote)
            .since(records[1].created_at)
            .until(records[3].created_at);
        let snapshot = relay.negentropy_snapshot(filter.clone()).await.unwrap();
        assert_eq!(snapshot.len(), 3);
        let id = SubscriptionId::new("immutable");
        let mut book = Sessions::default();
        let generation = book.reserve(1, &id).unwrap();
        book.install(1, &id, generation, &snapshot).unwrap();
        let later = EventBuilder::new(Kind::TextNote, "arrived after snapshot")
            .custom_created_at(records[2].created_at)
            .sign_with_keys(&keys)
            .unwrap();
        relay.ingest_trusted_event_silent(later).await.unwrap();
        assert_eq!(relay.negentropy_snapshot(filter).await.unwrap().len(), 4);
        assert_eq!(roundtrip(&mut book, 1, &id, &[]).len(), 3);
        assert!(relay
            .negentropy_snapshot(Filter::new().author(Keys::generate().public_key()))
            .await
            .unwrap()
            .is_empty());
        assert!(relay
            .negentropy_snapshot(Filter::new().kind(Kind::ContactList))
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn negentropy_rejects_oversized_snapshots_instead_of_advertising_partial_sets() {
        let directory = tempfile::tempdir().unwrap();
        let config = super::super::NostrRelayConfig {
            max_query_limit: MAX_RECORDS,
            ..Default::default()
        };
        let relay = NostrRelay::new_read_only(directory.path().to_path_buf(), config);
        for event in events(MAX_RECORDS + 1) {
            relay.ingest_trusted_event_silent(event).await.unwrap();
        }
        let (tx, mut rx) = mpsc::unbounded_channel();
        relay.register_client(7, tx, None).await;
        relay
            .handle_client_message(
                7,
                ClientMessage::NegOpen {
                    subscription_id: Cow::Owned(SubscriptionId::new("over-budget")),
                    filter: Cow::Owned(Filter::new()),
                    id_size: None,
                    initial_message: hex::encode(peer(&[]).initiate().unwrap()).into(),
                },
            )
            .await;
        match response(&mut rx).await {
            RelayMessage::NegErr { message, .. } => assert!(message.starts_with("blocked:")),
            other => panic!("must reject an incomplete set: {other:?}"),
        }
        assert!(relay.negentropy.lock().await.entries.is_empty());
    }

    fn events(count: usize) -> Vec<Event> {
        let keys = Keys::parse("0000000000000000000000000000000000000000000000000000000000000001")
            .unwrap();
        (0..count)
            .map(|i| {
                EventBuilder::new(Kind::TextNote, format!("fixture {i}"))
                    .custom_created_at(Timestamp::from_secs(1700000000 + (i / 3) as u64))
                    .sign_with_keys(&keys)
                    .unwrap()
            })
            .collect()
    }

    fn peer(events: &[Event]) -> Negentropy<'static, NegentropyStorageVector> {
        let mut storage = NegentropyStorageVector::new();
        for event in events {
            storage
                .insert(
                    event.created_at.as_secs(),
                    Id::from_byte_array(event.id.to_bytes()),
                )
                .unwrap();
        }
        storage.seal().unwrap();
        Negentropy::owned(storage, 4096).unwrap()
    }

    fn roundtrip(book: &mut Sessions, owner: u64, id: &SubscriptionId, local: &[Event]) -> Vec<Id> {
        let mut client = peer(local);
        let mut message = client.initiate().unwrap();
        let mut missing = Vec::new();
        for _ in 0..32 {
            let reply = book.respond(owner, id, &hex::encode(message)).unwrap();
            let next = client
                .reconcile_with_ids(&hex::decode(reply).unwrap(), &mut Vec::new(), &mut missing)
                .unwrap();
            match next {
                Some(next) => message = next,
                None => return missing,
            }
        }
        panic!("reconciliation did not finish");
    }

    #[test]
    fn negentropy_reconciles_fragmented_and_equal_timestamp_sets_then_warm_cache() {
        let records = events(600);
        let id = SubscriptionId::new("shared");
        let mut book = Sessions::default();
        let generation = book.reserve(1, &id).unwrap();
        assert!(book.install(1, &id, generation, &records).unwrap());
        let known = records.iter().step_by(2).cloned().collect::<Vec<_>>();
        let got = roundtrip(&mut book, 1, &id, &known)
            .into_iter()
            .collect::<std::collections::HashSet<_>>();
        let expected = records
            .iter()
            .skip(1)
            .step_by(2)
            .map(|e| Id::from_byte_array(e.id.to_bytes()))
            .collect();
        assert_eq!(got, expected);
        let generation = book.reserve(1, &id).unwrap();
        book.install(1, &id, generation, &records).unwrap();
        assert!(roundtrip(&mut book, 1, &id, &records).is_empty());
    }

    #[test]
    fn negentropy_sessions_are_private_to_clients_and_close_cannot_resurrect_a_snapshot() {
        let id = SubscriptionId::new("same");
        let records = events(2);
        let mut book = Sessions::default();
        let one = book.reserve(1, &id).unwrap();
        book.install(1, &id, one, &records[..1]).unwrap();
        let two = book.reserve(2, &id).unwrap();
        book.install(2, &id, two, &records[1..]).unwrap();
        assert_eq!(
            roundtrip(&mut book, 1, &id, &[]),
            vec![Id::from_byte_array(records[0].id.to_bytes())]
        );
        assert_eq!(
            roundtrip(&mut book, 2, &id, &[]),
            vec![Id::from_byte_array(records[1].id.to_bytes())]
        );
        let pending = book.reserve(1, &id).unwrap();
        book.disconnect(1);
        assert!(!book.install(1, &id, pending, &records).unwrap());
        assert!(book.entries.contains_key(&(2, id)));
    }

    #[test]
    fn negentropy_replacement_expiration_and_malformed_frames_release_state() {
        let id = SubscriptionId::new("replace");
        let mut book = Sessions::default();
        let old = book.reserve(1, &id).unwrap();
        let current = book.reserve(1, &id).unwrap();
        assert!(!book.install(1, &id, old, &[]).unwrap());
        book.install(1, &id, current, &[]).unwrap();
        assert!(book.respond(1, &id, "not-hex").is_err());
        assert!(!book.entries.contains_key(&(1, id.clone())));
        let current = book.reserve(1, &id).unwrap();
        book.install(1, &id, current, &[]).unwrap();
        book.entries.get_mut(&(1, id.clone())).unwrap().touched -= SESSION_TTL;
        assert!(book
            .respond(1, &id, "61")
            .unwrap_err()
            .starts_with("closed:"));
        assert!(book.entries.is_empty());
    }

    #[test]
    fn negentropy_bounds_sessions_records_messages_and_rounds() {
        let mut book = Sessions::default();
        let id = SubscriptionId::new("a");
        book.reserve(1, &id).unwrap();
        book.reserve(1, &SubscriptionId::new("b")).unwrap();
        assert!(book.reserve(1, &SubscriptionId::new("c")).is_err());
        for owner in 2..=32 {
            book.reserve(owner, &id).unwrap();
            book.reserve(owner, &SubscriptionId::new("b")).unwrap();
        }
        assert!(book.reserve(33, &id).is_err());
        book.disconnect(1);
        let generation = book.reserve(1, &id).unwrap();
        let one = events(1);
        assert!(book
            .install(1, &id, generation, &vec![one[0].clone(); MAX_RECORDS + 1])
            .is_err());
        book.install(1, &id, generation, &[]).unwrap();
        assert!(book.respond(1, &id, &"00".repeat(MAX_HEX_BYTES)).is_err());
        let generation = book.reserve(1, &id).unwrap();
        book.install(1, &id, generation, &[]).unwrap();
        book.entries.get_mut(&(1, id.clone())).unwrap().rounds = 32;
        assert!(book
            .respond(1, &id, "61")
            .unwrap_err()
            .starts_with("blocked:"));
    }
}
