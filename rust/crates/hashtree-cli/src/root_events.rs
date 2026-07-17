//! Nostr root-event parsing shared by the HTTP resolver.

use git_remote_htree::nostr_client::hashtree_root_kinds;
use nostr::{nips::nip19::FromBech32, Alphabet, Event, Filter, PublicKey, SingleLetterTag};

#[derive(Debug, Clone)]
pub struct PeerRootEvent {
    pub hash: String,
    pub key: Option<String>,
    pub encrypted_key: Option<String>,
    pub self_encrypted_key: Option<String>,
    pub event_id: String,
    pub created_at: u64,
    pub peer_id: String,
}

pub fn build_root_filter(owner_pubkey: &str, tree_name: &str) -> Option<Filter> {
    let author = PublicKey::from_hex(owner_pubkey)
        .or_else(|_| PublicKey::from_bech32(owner_pubkey))
        .ok()?;
    Some(
        Filter::new()
            .kinds(hashtree_root_kinds())
            .author(author)
            .custom_tag(
                SingleLetterTag::lowercase(Alphabet::D),
                tree_name.to_string(),
            )
            .custom_tag(SingleLetterTag::lowercase(Alphabet::L), "hashtree")
            .limit(50),
    )
}

pub fn pick_latest_event<'a, I>(events: I) -> Option<&'a Event>
where
    I: IntoIterator<Item = &'a Event>,
{
    events.into_iter().max_by(|a, b| {
        let ordering = a.created_at.cmp(&b.created_at);
        if ordering == std::cmp::Ordering::Equal {
            b.id.cmp(&a.id)
        } else {
            ordering
        }
    })
}

pub fn root_event_from_peer(
    event: &Event,
    peer_id: &str,
    tree_name: &str,
) -> Option<PeerRootEvent> {
    let mut tree_match = false;
    let mut labeled = false;
    let mut key = None;
    let mut encrypted_key = None;
    let mut self_encrypted_key = None;
    let mut hash_tag = None;

    for tag in event.tags.iter() {
        let slice = tag.as_slice();
        if slice.len() < 2 {
            continue;
        }
        match slice[0].as_str() {
            "d" => tree_match = slice[1].as_str() == tree_name,
            "l" => labeled |= slice[1].as_str() == "hashtree",
            "hash" => hash_tag = Some(slice[1].to_string()),
            "key" => key = Some(slice[1].to_string()),
            "encryptedKey" => encrypted_key = Some(slice[1].to_string()),
            "selfEncryptedKey" => self_encrypted_key = Some(slice[1].to_string()),
            _ => {}
        }
    }

    if !tree_match || !labeled {
        return None;
    }

    let hash = hash_tag.or_else(|| {
        if event.content.is_empty() {
            None
        } else {
            Some(event.content.clone())
        }
    })?;

    Some(PeerRootEvent {
        hash,
        key,
        encrypted_key,
        self_encrypted_key,
        event_id: event.id.to_hex(),
        created_at: event.created_at.as_secs(),
        peer_id: peer_id.to_string(),
    })
}
