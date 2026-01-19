//! Nostr relay transport implementation
//!
//! Wraps nostr-sdk Client to implement the RelayTransport trait for production use.
//! Uses NIP-17 style gift-wrapping for directed messages (offer, answer, candidate)
//! to provide privacy from relays.

use async_trait::async_trait;
use nostr_sdk::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{broadcast, Mutex};
use tracing::{debug, info, warn};

use crate::transport::{RelayTransport, TransportError};
use crate::types::{SignalingMessage, NOSTR_KIND_HASHTREE};

/// Hello tag for broadcast peer discovery
const HELLO_TAG: &str = "hello";

/// Nostr relay transport for production WebRTC signaling
pub struct NostrRelayTransport {
    /// Our peer ID (pubkey:uuid)
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
    /// Create a new Nostr relay transport with its own client
    pub fn new(keys: Keys, peer_uuid: String, debug: bool) -> Self {
        // Create client with in-memory database to avoid event deduplication
        let client = ClientBuilder::new()
            .signer(keys.clone())
            .database(nostr_sdk::database::MemoryDatabase::new())
            .build();

        Self::with_client(client, keys, peer_uuid, debug)
    }

    /// Create a new Nostr relay transport with an existing client
    ///
    /// This allows sharing the same relay connection pool with other components
    /// (e.g., Tauri's NostrManager). The client should already have relays added
    /// but connect() will be called when RelayTransport::connect() is invoked.
    pub fn with_client(client: Client, keys: Keys, peer_uuid: String, debug: bool) -> Self {
        let pubkey = keys.public_key().to_hex();
        let peer_id = format!("{}:{}", pubkey, peer_uuid);

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
        let debug = self.debug;

        // Get notifications receiver
        let mut notifications = self.client.notifications();

        tokio::spawn(async move {
            debug!("[NostrTransport] Event handler started");
            loop {
                match notifications.recv().await {
                    Ok(notification) => {
                        if let RelayPoolNotification::Event { event, .. } = notification {
                            if event.kind == Kind::Custom(NOSTR_KIND_HASHTREE) {
                                info!("[NostrTransport] Received kind={} event from {}", NOSTR_KIND_HASHTREE, &event.pubkey.to_hex()[..8]);
                                // Handle the event - may be hello (plain) or directed (encrypted)
                                if let Some(msg) = Self::handle_event(&event, &peer_id, &pubkey, &keys, debug) {
                                    info!("[NostrTransport] Forwarding message to recv channel: {}",
                                        match &msg {
                                            SignalingMessage::Hello { .. } => "hello",
                                            SignalingMessage::Offer { .. } => "offer",
                                            SignalingMessage::Answer { .. } => "answer",
                                            SignalingMessage::Candidate { .. } => "candidate",
                                            SignalingMessage::Candidates { .. } => "candidates",
                                        }
                                    );
                                    let _ = msg_tx.send(msg);
                                }
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        debug!("[NostrTransport] Event handler closed");
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

    /// Handle incoming event - returns SignalingMessage if valid and for us
    fn handle_event(
        event: &Event,
        my_peer_id: &str,
        my_pubkey: &str,
        keys: &Keys,
        _debug: bool,
    ) -> Option<SignalingMessage> {
        // Helper to get tag value
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

        // Check if this is a hello message (#l: "hello" tag)
        if get_tag("l").as_deref() == Some(HELLO_TAG) {
            let sender_pubkey = event.pubkey.to_hex();

            // Skip our own hello messages
            if sender_pubkey == my_pubkey {
                return None;
            }

            if let Some(their_uuid) = get_tag("peerId") {
                let their_peer_id = format!("{}:{}", sender_pubkey, their_uuid);
                info!("[NostrTransport] Received hello from {}", &sender_pubkey[..8.min(sender_pubkey.len())]);
                return Some(SignalingMessage::Hello {
                    peer_id: their_peer_id,
                    roots: vec![],
                });
            }
            return None;
        }

        // Check if this is a directed message for us (#p tag with our pubkey)
        let p_tag = get_tag("p");
        if p_tag.as_deref() != Some(my_pubkey) {
            // Not for us - ignore silently
            return None;
        }

        // Gift-wrapped directed message - decrypt using our key and ephemeral sender's pubkey
        if event.content.is_empty() {
            return None;
        }

        // Try to unwrap the gift - decrypt with our key and the ephemeral sender's pubkey
        let seal: serde_json::Value = match nip44::decrypt(keys.secret_key(), &event.pubkey, &event.content) {
            Ok(plaintext) => {
                match serde_json::from_str(&plaintext) {
                    Ok(v) => v,
                    Err(_) => return None,
                }
            }
            Err(_) => {
                // Can't decrypt - not for us or invalid
                return None;
            }
        };

        // Extract the actual sender's pubkey from the seal
        let sender_pubkey = seal.get("pubkey")
            .and_then(|v| v.as_str())?;

        // Skip our own messages
        if sender_pubkey == my_pubkey {
            return None;
        }

        let content = seal.get("content")
            .and_then(|v| v.as_str())?;

        let msg: SignalingMessage = serde_json::from_str(content).ok()?;

        info!(
            "[NostrTransport] Received {} from {} (gift-wrapped)",
            match &msg {
                SignalingMessage::Hello { .. } => "hello",
                SignalingMessage::Offer { .. } => "offer",
                SignalingMessage::Answer { .. } => "answer",
                SignalingMessage::Candidate { .. } => "candidate",
                SignalingMessage::Candidates { .. } => "candidates",
            },
            &sender_pubkey[..8.min(sender_pubkey.len())]
        );

        // Only forward if message is for us
        if msg.is_for(my_peer_id) {
            Some(msg)
        } else {
            None
        }
    }

    /// Get the nostr client (for advanced usage)
    pub fn client(&self) -> &Client {
        &self.client
    }
}

#[async_trait]
impl RelayTransport for NostrRelayTransport {
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
                vec![HELLO_TAG],
            )
            .since(Timestamp::now() - Duration::from_secs(60));

        let directed_filter = Filter::new()
            .kind(Kind::Custom(NOSTR_KIND_HASHTREE))
            .custom_tag(
                nostr_sdk::SingleLetterTag::lowercase(nostr_sdk::Alphabet::P),
                vec![self.pubkey.clone()],
            )
            .since(Timestamp::now() - Duration::from_secs(60));

        self.client
            .subscribe(vec![hello_filter, directed_filter], None)
            .await
            .map_err(|e| TransportError::ConnectionFailed(e.to_string()))?;

        info!("[NostrTransport] Subscriptions created for kind={}", NOSTR_KIND_HASHTREE);

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

        // Check if message has a target (needs gift wrapping)
        if let Some(target_peer_id) = msg.target_peer_id() {
            // Parse target peer ID to get their pubkey (format: pubkey:uuid)
            let recipient_pubkey = target_peer_id
                .split(':')
                .next()
                .ok_or_else(|| TransportError::SendFailed("Invalid target peer ID format".to_string()))?;

            let recipient_pk = PublicKey::from_hex(recipient_pubkey)
                .map_err(|e| TransportError::SendFailed(format!("Invalid recipient pubkey: {}", e)))?;

            // Create seal with sender's actual pubkey (the "rumor")
            let seal = serde_json::json!({
                "pubkey": self.pubkey,
                "kind": NOSTR_KIND_HASHTREE,
                "content": serde_json::to_string(&msg)
                    .map_err(|e| TransportError::SendFailed(e.to_string()))?,
                "tags": []
            });

            // Generate ephemeral keypair for the wrapper
            let ephemeral_keys = Keys::generate();

            // Encrypt the seal for the recipient using ephemeral key (NIP-44)
            let encrypted_content = nip44::encrypt(
                ephemeral_keys.secret_key(),
                &recipient_pk,
                &seal.to_string(),
                nip44::Version::V2
            ).map_err(|e| TransportError::SendFailed(format!("Encryption failed: {}", e)))?;

            // Create wrapper event with ephemeral key
            let expiration = Timestamp::now() + Duration::from_secs(5 * 60); // 5 minutes

            let tags = vec![
                Tag::public_key(recipient_pk),
                Tag::expiration(expiration),
            ];

            info!(
                "[NostrTransport] Publishing {} to {} (gift-wrapped)",
                match &msg {
                    SignalingMessage::Hello { .. } => "hello",
                    SignalingMessage::Offer { .. } => "offer",
                    SignalingMessage::Answer { .. } => "answer",
                    SignalingMessage::Candidate { .. } => "candidate",
                    SignalingMessage::Candidates { .. } => "candidates",
                },
                &recipient_pubkey[..8.min(recipient_pubkey.len())]
            );

            let builder = EventBuilder::new(Kind::Custom(NOSTR_KIND_HASHTREE), encrypted_content, tags);
            let event = builder
                .to_event(&ephemeral_keys)
                .map_err(|e| TransportError::SendFailed(e.to_string()))?;

            match self.client.send_event(event).await {
                Ok(output) => {
                    if output.success.is_empty() {
                        warn!("[NostrTransport] Directed message rejected - no relay accepted");
                        return Err(TransportError::SendFailed("No relay accepted event".to_string()));
                    }
                    info!("[NostrTransport] Directed message sent to {} relays", output.success.len());
                    Ok(())
                }
                Err(e) => {
                    warn!("[NostrTransport] Directed message send error: {}", e);
                    Err(TransportError::SendFailed(e.to_string()))
                }
            }
        } else {
            // Hello message - broadcast with #l: "hello" tag
            // Extract UUID from our peer_id (format: pubkey:uuid)
            let our_uuid = self.peer_id
                .split(':')
                .nth(1)
                .unwrap_or(&self.peer_id);

            debug!("[NostrTransport] Publishing hello (kind={}, uuid={}, pubkey={})", NOSTR_KIND_HASHTREE, our_uuid, &self.pubkey[..8]);

            // Add expiration tag (5 minutes) to match browser behavior
            let expiration = Timestamp::now() + Duration::from_secs(5 * 60);
            let tags = vec![
                Tag::custom(
                    nostr_sdk::TagKind::SingleLetter(nostr_sdk::SingleLetterTag::lowercase(nostr_sdk::Alphabet::L)),
                    vec![HELLO_TAG.to_string()]
                ),
                Tag::custom(
                    nostr_sdk::TagKind::Custom(std::borrow::Cow::Borrowed("peerId")),
                    vec![our_uuid.to_string()]
                ),
                Tag::expiration(expiration),
            ];

            let builder = EventBuilder::new(Kind::Custom(NOSTR_KIND_HASHTREE), "", tags);

            // Sign with our identity keys (not the client's signer which may be different)
            let event = builder
                .to_event(&self.keys)
                .map_err(|e| TransportError::SendFailed(format!("Failed to sign hello: {}", e)))?;

            match self.client.send_event(event).await {
                Ok(output) => {
                    if output.success.is_empty() {
                        warn!("[NostrTransport] Hello rejected - no relay accepted event");
                        return Err(TransportError::SendFailed("No relay accepted event".to_string()));
                    }
                    info!("[NostrTransport] Hello sent successfully to {} relays", output.success.len());
                    Ok(())
                }
                Err(e) => {
                    warn!("[NostrTransport] Hello send error: {}", e);
                    Err(TransportError::SendFailed(e.to_string()))
                }
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

    fn pubkey(&self) -> &str {
        &self.pubkey
    }
}
