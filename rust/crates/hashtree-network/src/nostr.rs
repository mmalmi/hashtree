//! Nostr websocket signaling transport implementation
//!
//! Wraps `nostr-sdk` to implement the generic signaling transport for
//! production use over relay websockets.
//! Uses NIP-17 style gift-wrapping for directed messages (offer, answer, candidate)
//! to provide privacy from relays.

use async_trait::async_trait;
use nostr_sdk::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{broadcast, Mutex};
use tracing::{debug, info, warn};

use crate::transport::{SignalingTransport, TransportError};
use crate::types::{SignalingMessage, NOSTR_KIND_HASHTREE};

/// Hello tag for broadcast peer discovery
const HELLO_TAG: &str = "hello";
const HASH_GET_TAG: &str = "hashGet";
const NIP59_SEAL_KIND: u16 = 13;

fn decode_hash_get_tag(tag_value: Option<String>) -> bool {
    match tag_value.as_deref() {
        Some("0" | "false" | "FALSE" | "no" | "NO") => false,
        Some("1" | "true" | "TRUE" | "yes" | "YES") | None => true,
        Some(_) => true,
    }
}

/// Decode a Nostr signaling event into a mesh signaling message.
pub fn decode_signaling_event(
    event: &Event,
    my_peer_id: &str,
    my_pubkey: &str,
    keys: &Keys,
) -> Option<SignalingMessage> {
    if event.verify().is_err() {
        return None;
    }

    if event.kind != Kind::Custom(NOSTR_KIND_HASHTREE)
        && event.kind != Kind::from_u16(NOSTR_KIND_HASHTREE)
    {
        return None;
    }

    let get_tag = |name: &str| -> Option<String> {
        event.tags.iter().find_map(|tag| {
            let v: Vec<String> = tag.clone().to_vec();
            if v.len() >= 2 && v[0] == name {
                Some(v[1].clone())
            } else {
                None
            }
        })
    };
    if get_tag("l").as_deref() == Some(HELLO_TAG) {
        let sender_pubkey = event.pubkey.to_hex();
        if sender_pubkey == my_pubkey {
            return None;
        }

        let claimed_peer_id =
            get_tag("peerId").and_then(|value| crate::types::PeerId::from_peer_string(&value))?;
        if claimed_peer_id.pubkey != sender_pubkey {
            return None;
        }

        return Some(SignalingMessage::Hello {
            peer_id: sender_pubkey,
            roots: vec![],
            hash_get: decode_hash_get_tag(get_tag(HASH_GET_TAG)),
        });
    }

    if get_tag("p").as_deref() != Some(my_pubkey) || event.content.is_empty() {
        return None;
    }

    let plaintext = match nip44::decrypt(keys.secret_key(), &event.pubkey, &event.content) {
        Ok(plaintext) => plaintext,
        Err(_) => return None,
    };

    if let Ok(inner_event) = serde_json::from_str::<Event>(&plaintext) {
        return decode_signed_inner_event(inner_event, my_peer_id, my_pubkey);
    }

    let seal: serde_json::Value = serde_json::from_str(&plaintext).ok()?;
    if let Some(inner_event_value) = seal.get("event") {
        let inner_event = serde_json::from_value::<Event>(inner_event_value.clone()).ok()?;
        return decode_signed_inner_event(inner_event, my_peer_id, my_pubkey);
    }
    if let Some(seal_event_value) = seal.get("seal") {
        let seal_event = serde_json::from_value::<Event>(seal_event_value.clone()).ok()?;
        return decode_seal_event(seal_event, my_peer_id, my_pubkey, keys);
    }

    decode_legacy_directed_seal(seal, my_peer_id, my_pubkey, &event.pubkey.to_hex())
}

fn decode_signed_inner_event(
    inner_event: Event,
    my_peer_id: &str,
    my_pubkey: &str,
) -> Option<SignalingMessage> {
    if inner_event.verify().is_err()
        || (inner_event.kind != Kind::Custom(NOSTR_KIND_HASHTREE)
            && inner_event.kind != Kind::from_u16(NOSTR_KIND_HASHTREE))
    {
        return None;
    }
    let sender_pubkey = inner_event.pubkey.to_hex();
    if sender_pubkey == my_pubkey {
        return None;
    }

    let msg = normalize_signaling_message(
        serde_json::from_str(&inner_event.content).ok()?,
        &sender_pubkey,
    )?;
    let claimed_pubkey = crate::types::PeerId::from_peer_string(msg.peer_id())?.pubkey;
    if claimed_pubkey.is_empty() || claimed_pubkey != sender_pubkey {
        return None;
    }

    msg.is_for(my_peer_id).then_some(msg)
}

fn decode_seal_event(
    seal_event: Event,
    my_peer_id: &str,
    my_pubkey: &str,
    keys: &Keys,
) -> Option<SignalingMessage> {
    if seal_event.verify().is_err() || seal_event.kind != Kind::Custom(NIP59_SEAL_KIND) {
        return None;
    }
    let sender_pubkey = seal_event.pubkey.to_hex();
    if sender_pubkey == my_pubkey {
        return None;
    }

    let plaintext =
        nip44::decrypt(keys.secret_key(), &seal_event.pubkey, &seal_event.content).ok()?;
    let rumor = UnsignedEvent::from_json(plaintext).ok()?;
    let msg =
        normalize_signaling_message(serde_json::from_str(&rumor.content).ok()?, &sender_pubkey)?;
    let claimed_pubkey = crate::types::PeerId::from_peer_string(msg.peer_id())?.pubkey;
    if claimed_pubkey.is_empty() || claimed_pubkey != sender_pubkey {
        return None;
    }

    msg.is_for(my_peer_id).then_some(msg)
}

fn decode_legacy_directed_seal(
    seal: serde_json::Value,
    my_peer_id: &str,
    my_pubkey: &str,
    outer_pubkey: &str,
) -> Option<SignalingMessage> {
    let sender_pubkey = seal.get("pubkey").and_then(|v| v.as_str())?;
    if sender_pubkey != outer_pubkey {
        return None;
    }
    if sender_pubkey == my_pubkey {
        return None;
    }

    let content = seal.get("content").and_then(|v| v.as_str())?;
    let msg = normalize_signaling_message(serde_json::from_str(content).ok()?, sender_pubkey)?;
    let claimed_pubkey = crate::types::PeerId::from_peer_string(msg.peer_id())?.pubkey;
    if claimed_pubkey.is_empty() || claimed_pubkey != sender_pubkey {
        return None;
    }

    msg.is_for(my_peer_id).then_some(msg)
}

/// Encode a mesh signaling message as a Nostr event.
pub fn encode_signaling_event(
    keys: &Keys,
    local_peer_id: &str,
    msg: &SignalingMessage,
    kind: Kind,
) -> Result<Event, TransportError> {
    if let Some(target_peer_id) = msg.target_peer_id() {
        let recipient_pubkey = crate::types::PeerId::from_peer_string(target_peer_id)
            .map(|peer_id| peer_id.pubkey)
            .ok_or_else(|| {
                TransportError::SendFailed("Invalid target peer ID format".to_string())
            })?;
        let recipient_pk = PublicKey::from_hex(&recipient_pubkey)
            .map_err(|e| TransportError::SendFailed(format!("Invalid recipient pubkey: {e}")))?;

        let expiration = Timestamp::now() + Duration::from_secs(5 * 60);
        let message_json =
            serde_json::to_string(msg).map_err(|e| TransportError::SendFailed(e.to_string()))?;
        let rumor = EventBuilder::new(kind, message_json).build(keys.public_key());
        let seal_content = nip44::encrypt(
            keys.secret_key(),
            &recipient_pk,
            rumor.as_json(),
            nip44::Version::V2,
        )
        .map_err(|e| TransportError::SendFailed(format!("Failed to encrypt seal: {e}")))?;
        let seal_event = EventBuilder::new(Kind::Seal, seal_content)
            .sign_with_keys(keys)
            .map_err(|e| TransportError::SendFailed(format!("Failed to sign seal: {e}")))?;
        let seal = serde_json::json!({
            "pubkey": keys.public_key().to_hex(),
            "kind": NOSTR_KIND_HASHTREE,
            "content": rumor.content,
            "tags": [],
            "seal": seal_event,
        });

        let ephemeral_keys = Keys::generate();
        let encrypted_content = nip44::encrypt(
            ephemeral_keys.secret_key(),
            &recipient_pk,
            seal.to_string(),
            nip44::Version::V2,
        )
        .map_err(|e| TransportError::SendFailed(format!("Encryption failed: {e}")))?;

        let tags = vec![Tag::public_key(recipient_pk), Tag::expiration(expiration)];

        return EventBuilder::new(kind, encrypted_content)
            .tags(tags)
            .sign_with_keys(&ephemeral_keys)
            .map_err(|e| TransportError::SendFailed(e.to_string()));
    }

    let hash_get = match msg {
        SignalingMessage::Hello { hash_get, .. } => *hash_get,
        _ => true,
    };
    let expiration = Timestamp::now() + Duration::from_secs(5 * 60);
    let tags = vec![
        Tag::custom(
            nostr_sdk::TagKind::SingleLetter(nostr_sdk::SingleLetterTag::lowercase(
                nostr_sdk::Alphabet::L,
            )),
            vec![HELLO_TAG.to_string()],
        ),
        Tag::custom(
            nostr_sdk::TagKind::Custom(std::borrow::Cow::Borrowed("peerId")),
            vec![local_peer_id.to_string()],
        ),
        Tag::custom(
            nostr_sdk::TagKind::Custom(std::borrow::Cow::Borrowed(HASH_GET_TAG)),
            vec![if hash_get { "1" } else { "0" }.to_string()],
        ),
        Tag::expiration(expiration),
    ];

    EventBuilder::new(kind, "")
        .tags(tags)
        .sign_with_keys(keys)
        .map_err(|e| TransportError::SendFailed(format!("Failed to sign hello: {e}")))
}

/// Nostr websocket signaling transport for production use.
pub struct NostrRelayTransport {
    /// Our peer ID (pubkey)
    peer_id: String,
    /// Our pubkey (hex)
    pubkey: String,
    /// Nostr keys for signing and decryption
    keys: Keys,
    /// Nostr client
    client: Client,
    /// Message buffer for received signaling messages
    buffer: Mutex<Vec<SignalingMessage>>,
    /// Whether we're connected
    connected: AtomicBool,
    /// Receiver for messages from event handler
    msg_rx: Mutex<Option<broadcast::Receiver<SignalingMessage>>>,
    /// Sender for forwarding messages from event handler
    msg_tx: broadcast::Sender<SignalingMessage>,
    /// Debug flag
    debug: bool,
}

impl NostrRelayTransport {
    /// Create a new Nostr signaling transport with its own client.
    pub fn new(keys: Keys, debug: bool) -> Self {
        // Create client with in-memory database to avoid event deduplication
        let client = ClientBuilder::new().signer(keys.clone()).build();

        Self::with_client(client, keys, debug)
    }

    /// Create a new Nostr relay transport with an existing client
    ///
    /// This allows sharing the same relay connection pool with other components
    /// (e.g., Tauri's NostrManager). The client should already have relays added
    /// but `connect()` will be called when the signaling transport is started.
    pub fn with_client(client: Client, keys: Keys, debug: bool) -> Self {
        let pubkey = keys.public_key().to_hex();
        let peer_id = pubkey.clone();

        let (msg_tx, msg_rx) = broadcast::channel(1000);

        Self {
            peer_id,
            pubkey,
            keys,
            client,
            buffer: Mutex::new(Vec::new()),
            connected: AtomicBool::new(false),
            msg_rx: Mutex::new(Some(msg_rx)),
            msg_tx,
            debug,
        }
    }

    /// Start the background event handler
    fn start_event_handler(&self) {
        let msg_tx = self.msg_tx.clone();
        let peer_id = self.peer_id.clone();
        let pubkey = self.pubkey.clone();
        let keys = self.keys.clone();
        let debug_enabled = self.debug;
        // Get notifications receiver
        let mut notifications = self.client.notifications();

        tokio::spawn(async move {
            if debug_enabled {
                debug!("[NostrTransport] Event handler started");
            }
            loop {
                match notifications.recv().await {
                    Ok(notification) => {
                        if let RelayPoolNotification::Event { event, .. } = notification {
                            if event.kind == Kind::Custom(NOSTR_KIND_HASHTREE)
                                || event.kind == Kind::from_u16(NOSTR_KIND_HASHTREE)
                            {
                                info!(
                                    "[NostrTransport] Received kind={} event from {}",
                                    NOSTR_KIND_HASHTREE,
                                    &event.pubkey.to_hex()[..8]
                                );
                                // Handle the event - may be hello (plain) or directed (encrypted)
                                if let Some(msg) =
                                    decode_signaling_event(&event, &peer_id, &pubkey, &keys)
                                {
                                    info!(
                                        "[NostrTransport] Forwarding message to recv channel: {}",
                                        msg.msg_type()
                                    );
                                    let _ = msg_tx.send(msg);
                                }
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        if debug_enabled {
                            debug!("[NostrTransport] Event handler closed");
                        }
                        break;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("[NostrTransport] Event handler lagged by {} messages", n);
                        continue;
                    }
                }
            }
        });
    }

    /// Get the nostr client (for advanced usage)
    pub fn client(&self) -> &Client {
        &self.client
    }
}

#[async_trait]
impl SignalingTransport for NostrRelayTransport {
    async fn connect(&self, relays: &[String]) -> Result<(), TransportError> {
        // Add relays
        for relay in relays {
            self.client
                .add_relay(relay)
                .await
                .map_err(|e| TransportError::ConnectionFailed(e.to_string()))?;
        }

        // Connect
        info!("[NostrTransport] Connecting to relays...");
        self.client.connect().await;
        info!("[NostrTransport] Connected, setting up subscriptions...");

        // Subscribe to hashtree signaling events - two filters:
        // 1. Hello messages: kind with #l: "hello" tag (broadcasts)
        // 2. Directed messages: kind with #p tag (our pubkey) for gift-wrapped messages
        let hello_filter = Filter::new()
            .kind(Kind::Custom(NOSTR_KIND_HASHTREE))
            .custom_tag(
                nostr_sdk::SingleLetterTag::lowercase(nostr_sdk::Alphabet::L),
                HELLO_TAG,
            )
            .since(Timestamp::now() - Duration::from_secs(60));

        let directed_filter = Filter::new()
            .kind(Kind::Custom(NOSTR_KIND_HASHTREE))
            .custom_tag(
                nostr_sdk::SingleLetterTag::lowercase(nostr_sdk::Alphabet::P),
                self.pubkey.clone(),
            )
            .since(Timestamp::now() - Duration::from_secs(60));

        self.client
            .subscribe(hello_filter, None)
            .await
            .map_err(|e| TransportError::ConnectionFailed(e.to_string()))?;
        self.client
            .subscribe(directed_filter, None)
            .await
            .map_err(|e| TransportError::ConnectionFailed(e.to_string()))?;

        info!(
            "[NostrTransport] Subscriptions created for kind={}",
            NOSTR_KIND_HASHTREE
        );

        // Start event handler
        self.start_event_handler();

        self.connected.store(true, Ordering::Relaxed);
        info!("[NostrTransport] Transport connected and ready");
        Ok(())
    }

    async fn disconnect(&self) {
        self.connected.store(false, Ordering::Relaxed);
        let _ = self.client.disconnect().await;
    }

    async fn publish(&self, msg: SignalingMessage) -> Result<(), TransportError> {
        if !self.connected.load(Ordering::Relaxed) {
            return Err(TransportError::NotConnected);
        }

        if msg.target_peer_id().is_some() {
            let recipient_pubkey = msg
                .target_peer_id()
                .and_then(crate::types::PeerId::from_peer_string)
                .map(|peer_id| peer_id.pubkey)
                .unwrap_or_default();
            info!(
                "[NostrTransport] Publishing {} to {} (gift-wrapped)",
                msg.msg_type(),
                &recipient_pubkey[..8.min(recipient_pubkey.len())]
            );
        } else {
            debug!(
                "[NostrTransport] Publishing hello (kind={}, peer_id={}, pubkey={})",
                NOSTR_KIND_HASHTREE,
                self.peer_id,
                &self.pubkey[..8]
            );
        }

        let event = encode_signaling_event(
            &self.keys,
            &self.peer_id,
            &msg,
            Kind::Custom(NOSTR_KIND_HASHTREE),
        )?;

        match self.client.send_event(&event).await {
            Ok(output) => {
                if output.success.is_empty() {
                    warn!(
                        "[NostrTransport] {} rejected - no relay accepted event",
                        msg.msg_type()
                    );
                    return Err(TransportError::SendFailed(
                        "No relay accepted event".to_string(),
                    ));
                }
                info!(
                    "[NostrTransport] {} sent successfully to {} relays",
                    msg.msg_type(),
                    output.success.len()
                );
                Ok(())
            }
            Err(e) => {
                warn!("[NostrTransport] {} send error: {}", msg.msg_type(), e);
                Err(TransportError::SendFailed(e.to_string()))
            }
        }
    }

    async fn recv(&self) -> Option<SignalingMessage> {
        // Check buffer first
        {
            let mut buffer = self.buffer.lock().await;
            if !buffer.is_empty() {
                return Some(buffer.remove(0));
            }
        }

        // Take the receiver if we have it
        let rx = self.msg_rx.lock().await.take();
        if let Some(mut rx) = rx {
            loop {
                match rx.recv().await {
                    Ok(msg) => {
                        // Put receiver back for next call
                        *self.msg_rx.lock().await = Some(rx);
                        return Some(msg);
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        }
        None
    }

    fn try_recv(&self) -> Option<SignalingMessage> {
        // Check buffer first
        if let Ok(mut buffer) = self.buffer.try_lock() {
            if !buffer.is_empty() {
                return Some(buffer.remove(0));
            }
        }

        // Try non-blocking receive
        if let Ok(mut rx_guard) = self.msg_rx.try_lock() {
            if let Some(ref mut rx) = *rx_guard {
                match rx.try_recv() {
                    Ok(msg) => return Some(msg),
                    Err(_) => return None,
                }
            }
        }
        None
    }

    fn peer_id(&self) -> &str {
        &self.peer_id
    }
}

fn normalize_signaling_message(
    msg: SignalingMessage,
    sender_pubkey: &str,
) -> Option<SignalingMessage> {
    let sender_peer_id = crate::types::PeerId::from_peer_string(sender_pubkey)
        .unwrap_or_else(|| crate::types::PeerId::new(sender_pubkey.to_string()))
        .to_string();

    Some(match msg {
        SignalingMessage::Hello {
            roots, hash_get, ..
        } => SignalingMessage::Hello {
            peer_id: sender_peer_id,
            roots,
            hash_get,
        },
        SignalingMessage::Offer {
            target_peer_id,
            sdp,
            ..
        } => SignalingMessage::Offer {
            peer_id: sender_peer_id,
            target_peer_id: crate::types::PeerId::from_peer_string(&target_peer_id)?.to_string(),
            sdp,
        },
        SignalingMessage::Answer {
            target_peer_id,
            sdp,
            ..
        } => SignalingMessage::Answer {
            peer_id: sender_peer_id,
            target_peer_id: crate::types::PeerId::from_peer_string(&target_peer_id)?.to_string(),
            sdp,
        },
        SignalingMessage::Candidate {
            target_peer_id,
            candidate,
            sdp_m_line_index,
            sdp_mid,
            ..
        } => SignalingMessage::Candidate {
            peer_id: sender_peer_id,
            target_peer_id: crate::types::PeerId::from_peer_string(&target_peer_id)?.to_string(),
            candidate,
            sdp_m_line_index,
            sdp_mid,
        },
        SignalingMessage::Candidates {
            target_peer_id,
            candidates,
            ..
        } => SignalingMessage::Candidates {
            peer_id: sender_peer_id,
            target_peer_id: crate::types::PeerId::from_peer_string(&target_peer_id)?.to_string(),
            candidates,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directed_events_round_trip_with_target_peer_id_shape() {
        let sender_keys = Keys::generate();
        let recipient_keys = Keys::generate();
        let sender_peer_id = sender_keys.public_key().to_hex();
        let recipient_peer_id = recipient_keys.public_key().to_hex();
        let msg = SignalingMessage::Offer {
            peer_id: sender_peer_id.clone(),
            target_peer_id: recipient_peer_id.clone(),
            sdp: "test-sdp".to_string(),
        };

        let event = encode_signaling_event(
            &sender_keys,
            &sender_peer_id,
            &msg,
            Kind::from_u16(NOSTR_KIND_HASHTREE),
        )
        .expect("encode signaling event");

        let plaintext = nip44::decrypt(recipient_keys.secret_key(), &event.pubkey, &event.content)
            .expect("decrypt directed signaling");
        let seal: serde_json::Value =
            serde_json::from_str(&plaintext).expect("decode directed signaling seal");
        assert_eq!(
            seal.get("pubkey").and_then(|value| value.as_str()),
            Some(sender_peer_id.as_str())
        );
        let legacy_content = seal
            .get("content")
            .and_then(|value| value.as_str())
            .expect("legacy content");
        assert!(legacy_content.contains("\"targetPeerId\""));
        assert!(!legacy_content.contains("\"recipient\""));
        let seal_event: Event =
            serde_json::from_value(seal.get("seal").cloned().expect("signed seal event"))
                .expect("decode seal event");
        assert!(seal_event.verify().is_ok());
        assert_eq!(seal_event.kind, Kind::Custom(NIP59_SEAL_KIND));
        assert_eq!(seal_event.pubkey, sender_keys.public_key());
        let rumor_json = nip44::decrypt(
            recipient_keys.secret_key(),
            &seal_event.pubkey,
            &seal_event.content,
        )
        .expect("decrypt rumor");
        let rumor_value: serde_json::Value = serde_json::from_str(&rumor_json).expect("rumor json");
        assert!(rumor_value.get("sig").is_none());
        let rumor = UnsignedEvent::from_json(rumor_json).expect("decode rumor");
        assert!(rumor.content.contains("\"targetPeerId\""));
        assert!(!rumor.content.contains("\"recipient\""));

        let decoded = decode_signaling_event(
            &event,
            &recipient_peer_id,
            &recipient_keys.public_key().to_hex(),
            &recipient_keys,
        )
        .expect("decode directed signaling");

        match decoded {
            SignalingMessage::Offer {
                peer_id,
                target_peer_id,
                sdp,
            } => {
                assert_eq!(peer_id, sender_peer_id);
                assert_eq!(target_peer_id, recipient_peer_id);
                assert_eq!(sdp, "test-sdp");
            }
            other => panic!("expected offer, got {:?}", other),
        }
    }

    #[test]
    fn directed_decoder_uses_inner_event_signer_not_claimed_peer_id() {
        let attacker_keys = Keys::generate();
        let claimed_keys = Keys::generate();
        let recipient_keys = Keys::generate();
        let claimed_peer_id = claimed_keys.public_key().to_hex();
        let recipient_peer_id = recipient_keys.public_key().to_hex();
        let msg = SignalingMessage::Offer {
            peer_id: claimed_peer_id.clone(),
            target_peer_id: recipient_peer_id.clone(),
            sdp: "test-sdp".to_string(),
        };

        let event = encode_signaling_event(
            &attacker_keys,
            &attacker_keys.public_key().to_hex(),
            &msg,
            Kind::from_u16(NOSTR_KIND_HASHTREE),
        )
        .expect("encode signaling event");
        let decoded = decode_signaling_event(
            &event,
            &recipient_peer_id,
            &recipient_keys.public_key().to_hex(),
            &recipient_keys,
        )
        .expect("decode directed signaling");

        assert_eq!(decoded.peer_id(), attacker_keys.public_key().to_hex());
        assert_ne!(decoded.peer_id(), claimed_peer_id);
    }

    #[test]
    fn decoder_accepts_legacy_directed_seal_for_transition() {
        let sender_keys = Keys::generate();
        let recipient_keys = Keys::generate();
        let sender_peer_id = sender_keys.public_key().to_hex();
        let recipient_peer_id = recipient_keys.public_key().to_hex();
        let msg = SignalingMessage::Offer {
            peer_id: sender_peer_id.clone(),
            target_peer_id: recipient_peer_id.clone(),
            sdp: "legacy-sdp".to_string(),
        };
        let seal = serde_json::json!({
            "pubkey": sender_peer_id,
            "kind": NOSTR_KIND_HASHTREE,
            "content": serde_json::to_string(&msg).expect("legacy message json"),
            "tags": []
        });
        let recipient_pk =
            PublicKey::from_hex(&recipient_keys.public_key().to_hex()).expect("recipient pubkey");
        let ciphertext = nip44::encrypt(
            sender_keys.secret_key(),
            &recipient_pk,
            seal.to_string(),
            nip44::Version::V2,
        )
        .expect("encrypt legacy message");
        let event = EventBuilder::new(Kind::from_u16(NOSTR_KIND_HASHTREE), ciphertext)
            .tags(vec![
                Tag::public_key(recipient_pk),
                Tag::expiration(Timestamp::now() + Duration::from_secs(300)),
            ])
            .sign_with_keys(&sender_keys)
            .expect("build event");

        let decoded = decode_signaling_event(
            &event,
            &recipient_peer_id,
            &recipient_keys.public_key().to_hex(),
            &recipient_keys,
        )
        .expect("decode legacy directed signaling");
        assert_eq!(decoded.peer_id(), sender_keys.public_key().to_hex());
    }

    #[test]
    fn decoder_rejects_legacy_directed_seal_with_mismatched_outer_signer() {
        let attacker_keys = Keys::generate();
        let claimed_keys = Keys::generate();
        let recipient_keys = Keys::generate();
        let claimed_peer_id = claimed_keys.public_key().to_hex();
        let recipient_peer_id = recipient_keys.public_key().to_hex();
        let msg = SignalingMessage::Offer {
            peer_id: claimed_peer_id.clone(),
            target_peer_id: recipient_peer_id.clone(),
            sdp: "spoofed-legacy-sdp".to_string(),
        };
        let seal = serde_json::json!({
            "pubkey": claimed_peer_id,
            "kind": NOSTR_KIND_HASHTREE,
            "content": serde_json::to_string(&msg).expect("legacy message json"),
            "tags": []
        });
        let recipient_pk =
            PublicKey::from_hex(&recipient_keys.public_key().to_hex()).expect("recipient pubkey");
        let ciphertext = nip44::encrypt(
            attacker_keys.secret_key(),
            &recipient_pk,
            seal.to_string(),
            nip44::Version::V2,
        )
        .expect("encrypt spoofed legacy message");
        let event = EventBuilder::new(Kind::from_u16(NOSTR_KIND_HASHTREE), ciphertext)
            .tags(vec![
                Tag::public_key(recipient_pk),
                Tag::expiration(Timestamp::now() + Duration::from_secs(300)),
            ])
            .sign_with_keys(&attacker_keys)
            .expect("build spoofed event");

        assert!(decode_signaling_event(
            &event,
            &recipient_peer_id,
            &recipient_keys.public_key().to_hex(),
            &recipient_keys,
        )
        .is_none());
    }

    #[test]
    fn decoder_rejects_tampered_seal_without_legacy_fallback() {
        let sender_keys = Keys::generate();
        let recipient_keys = Keys::generate();
        let sender_peer_id = sender_keys.public_key().to_hex();
        let recipient_peer_id = recipient_keys.public_key().to_hex();
        let msg = SignalingMessage::Offer {
            peer_id: sender_peer_id,
            target_peer_id: recipient_peer_id.clone(),
            sdp: "test-sdp".to_string(),
        };
        let event = encode_signaling_event(
            &sender_keys,
            msg.peer_id(),
            &msg,
            Kind::from_u16(NOSTR_KIND_HASHTREE),
        )
        .expect("encode signaling event");
        let plaintext = nip44::decrypt(recipient_keys.secret_key(), &event.pubkey, &event.content)
            .expect("decrypt directed signaling");
        let mut seal: serde_json::Value =
            serde_json::from_str(&plaintext).expect("decode directed signaling seal");
        seal["seal"]["content"] = serde_json::Value::String("tampered".to_string());

        let recipient_pk =
            PublicKey::from_hex(&recipient_keys.public_key().to_hex()).expect("recipient pubkey");
        let ephemeral_keys = Keys::generate();
        let ciphertext = nip44::encrypt(
            ephemeral_keys.secret_key(),
            &recipient_pk,
            seal.to_string(),
            nip44::Version::V2,
        )
        .expect("encrypt tampered message");
        let tampered_event = EventBuilder::new(Kind::from_u16(NOSTR_KIND_HASHTREE), ciphertext)
            .tags(vec![
                Tag::public_key(recipient_pk),
                Tag::expiration(Timestamp::now() + Duration::from_secs(300)),
            ])
            .sign_with_keys(&ephemeral_keys)
            .expect("build tampered event");

        assert!(decode_signaling_event(
            &tampered_event,
            &recipient_peer_id,
            &recipient_keys.public_key().to_hex(),
            &recipient_keys,
        )
        .is_none());
    }

    #[test]
    fn hello_events_encode_and_decode_hash_get_capability() {
        let sender_keys = Keys::generate();
        let sender_peer_id = sender_keys.public_key().to_hex();
        let msg = SignalingMessage::Hello {
            peer_id: sender_peer_id.clone(),
            roots: vec![],
            hash_get: false,
        };

        let event = encode_signaling_event(
            &sender_keys,
            &sender_peer_id,
            &msg,
            Kind::from_u16(NOSTR_KIND_HASHTREE),
        )
        .expect("encode hello");

        let hash_get_tag = event
            .tags
            .iter()
            .find_map(|tag| {
                let values = tag.as_slice();
                (values.first().map(|value| value.as_str()) == Some("hashGet"))
                    .then(|| values.get(1).cloned())
                    .flatten()
            })
            .expect("hashGet tag");
        assert_eq!(hash_get_tag, "0");
        let address_tags: Vec<_> = event
            .tags
            .iter()
            .filter_map(|tag| {
                let values = tag.as_slice();
                (values.first().map(|value| value.as_str()) == Some("addr"))
                    .then(|| values.get(1).cloned())
                    .flatten()
            })
            .collect();
        assert!(address_tags.is_empty());

        let decoded =
            decode_signaling_event(&event, "receiver", "receiver-pubkey", &Keys::generate())
                .expect("decode hello");

        match decoded {
            SignalingMessage::Hello {
                peer_id, hash_get, ..
            } => {
                assert_eq!(peer_id, sender_peer_id);
                assert!(!hash_get);
            }
            other => panic!("expected hello, got {:?}", other),
        }
    }

    #[test]
    fn hello_decode_defaults_hash_get_to_true_when_tag_missing() {
        let sender_keys = Keys::generate();
        let sender_peer_id = sender_keys.public_key().to_hex();
        let event = EventBuilder::new(Kind::from_u16(NOSTR_KIND_HASHTREE), "")
            .tags(vec![
                Tag::custom(
                    nostr_sdk::TagKind::SingleLetter(nostr_sdk::SingleLetterTag::lowercase(
                        nostr_sdk::Alphabet::L,
                    )),
                    vec![HELLO_TAG.to_string()],
                ),
                Tag::custom(
                    nostr_sdk::TagKind::Custom(std::borrow::Cow::Borrowed("peerId")),
                    vec![sender_peer_id.clone()],
                ),
                Tag::expiration(Timestamp::now() + Duration::from_secs(300)),
            ])
            .sign_with_keys(&sender_keys)
            .expect("build hello event");

        let decoded =
            decode_signaling_event(&event, "receiver", "receiver-pubkey", &Keys::generate())
                .expect("decode hello");

        match decoded {
            SignalingMessage::Hello { hash_get, .. } => assert!(hash_get),
            other => panic!("expected hello, got {:?}", other),
        }
    }

    #[test]
    fn decoder_rejects_invalid_event_signature() {
        let sender_keys = Keys::generate();
        let sender_peer_id = sender_keys.public_key().to_hex();
        let msg = SignalingMessage::Hello {
            peer_id: sender_peer_id,
            roots: vec![],
            hash_get: true,
        };
        let event = encode_signaling_event(
            &sender_keys,
            msg.peer_id(),
            &msg,
            Kind::from_u16(NOSTR_KIND_HASHTREE),
        )
        .expect("encode hello");
        let mut tampered_value = serde_json::to_value(event).expect("event json");
        tampered_value["content"] = serde_json::Value::String("tampered".to_string());
        let tampered_event: Event = serde_json::from_value(tampered_value).expect("tampered event");

        assert!(tampered_event.verify().is_err());
        assert!(decode_signaling_event(
            &tampered_event,
            "receiver",
            "receiver-pubkey",
            &Keys::generate()
        )
        .is_none());
    }

    #[test]
    fn decoder_rejects_legacy_recipient_shape() {
        let sender_keys = Keys::generate();
        let recipient_keys = Keys::generate();
        let sender_peer_id = sender_keys.public_key().to_hex();
        let recipient_peer_id = recipient_keys.public_key().to_hex();
        let legacy_content = serde_json::json!({
            "type": "offer",
            "peerId": sender_peer_id,
            "recipient": recipient_peer_id,
            "offer": { "type": "offer", "sdp": "legacy-sdp" }
        })
        .to_string();
        let seal = serde_json::json!({
            "pubkey": sender_keys.public_key().to_hex(),
            "kind": NOSTR_KIND_HASHTREE,
            "content": legacy_content,
            "tags": []
        });

        let recipient_pk =
            PublicKey::from_hex(&recipient_keys.public_key().to_hex()).expect("recipient pubkey");
        let ephemeral_keys = Keys::generate();
        let ciphertext = nip44::encrypt(
            ephemeral_keys.secret_key(),
            &recipient_pk,
            seal.to_string(),
            nip44::Version::V2,
        )
        .expect("encrypt legacy message");
        let event = EventBuilder::new(Kind::from_u16(NOSTR_KIND_HASHTREE), ciphertext)
            .tags(vec![
                Tag::public_key(recipient_pk),
                Tag::expiration(Timestamp::now() + Duration::from_secs(300)),
            ])
            .sign_with_keys(&ephemeral_keys)
            .expect("build event");

        assert!(decode_signaling_event(
            &event,
            &recipient_peer_id,
            &recipient_keys.public_key().to_hex(),
            &recipient_keys,
        )
        .is_none());
    }
}
