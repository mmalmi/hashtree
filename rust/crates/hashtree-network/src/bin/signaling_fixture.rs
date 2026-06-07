use anyhow::{bail, Context, Result};
use hashtree_network::types::{SignalingMessage, NOSTR_KIND_HASHTREE};
use hashtree_network::{decode_signaling_event, encode_signaling_event};
use nostr_sdk::prelude::*;
use std::env;
use std::io::{self, Read};

const SENDER_SECRET_HEX: &str = "0101010101010101010101010101010101010101010101010101010101010101";
const RECIPIENT_SECRET_HEX: &str =
    "0202020202020202020202020202020202020202020202020202020202020202";
const INTEROP_SDP: &str = "v=0\r\ns=hashtree-signaling-interop\r\n";

fn keys_from_hex(secret_hex: &str) -> Result<Keys> {
    let secret_bytes = hex::decode(secret_hex).context("decode secret key hex")?;
    let secret = SecretKey::from_slice(&secret_bytes).context("parse secret key")?;
    Ok(Keys::new(secret))
}

fn interop_message(sender_peer_id: String, recipient_peer_id: String) -> SignalingMessage {
    SignalingMessage::Offer {
        peer_id: sender_peer_id,
        target_peer_id: recipient_peer_id,
        sdp: INTEROP_SDP.to_string(),
    }
}

fn encode_offer() -> Result<()> {
    let sender_keys = keys_from_hex(SENDER_SECRET_HEX)?;
    let recipient_keys = keys_from_hex(RECIPIENT_SECRET_HEX)?;
    let sender_peer_id = sender_keys.public_key().to_hex();
    let recipient_peer_id = recipient_keys.public_key().to_hex();
    let msg = interop_message(sender_peer_id.clone(), recipient_peer_id);
    let event = encode_signaling_event(
        &sender_keys,
        &sender_peer_id,
        &msg,
        Kind::from_u16(NOSTR_KIND_HASHTREE),
    )?;
    println!("{}", serde_json::to_string(&event)?);
    Ok(())
}

fn decode_offer() -> Result<()> {
    let recipient_keys = keys_from_hex(RECIPIENT_SECRET_HEX)?;
    let recipient_peer_id = recipient_keys.public_key().to_hex();
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .context("read event json")?;
    let event: Event = serde_json::from_str(&input).context("parse event json")?;
    let decoded = decode_signaling_event(
        &event,
        &recipient_peer_id,
        &recipient_keys.public_key().to_hex(),
        &recipient_keys,
    )
    .context("decode signaling event")?;
    println!("{}", serde_json::to_string(&decoded)?);
    Ok(())
}

fn print_keys() -> Result<()> {
    let sender_keys = keys_from_hex(SENDER_SECRET_HEX)?;
    let recipient_keys = keys_from_hex(RECIPIENT_SECRET_HEX)?;
    println!(
        "{}",
        serde_json::json!({
            "senderSecretHex": SENDER_SECRET_HEX,
            "senderPubkey": sender_keys.public_key().to_hex(),
            "recipientSecretHex": RECIPIENT_SECRET_HEX,
            "recipientPubkey": recipient_keys.public_key().to_hex(),
            "sdp": INTEROP_SDP,
        })
    );
    Ok(())
}

fn main() -> Result<()> {
    match env::args().nth(1).as_deref() {
        Some("encode-offer") => encode_offer(),
        Some("decode-offer") => decode_offer(),
        Some("keys") => print_keys(),
        Some(other) => bail!("unknown command: {other}"),
        None => bail!("usage: signaling_fixture <encode-offer|decode-offer|keys>"),
    }
}
