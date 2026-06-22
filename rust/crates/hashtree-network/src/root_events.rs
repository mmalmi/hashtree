use nostr_sdk::nostr::{
    nips::nip19::FromBech32, Alphabet, Event, Filter, Kind, PublicKey, SingleLetterTag,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRootEvent {
    pub hash: String,
    pub key: Option<String>,
    pub encrypted_key: Option<String>,
    pub self_encrypted_key: Option<String>,
    pub event_id: String,
    pub created_at: u64,
    pub peer_id: String,
}

pub const HASHTREE_KIND: u16 = 30064;
pub const HASHTREE_LEGACY_KIND: u16 = 30078;
pub const HASHTREE_LABEL: &str = "hashtree";

pub fn hashtree_root_kinds() -> Vec<Kind> {
    vec![
        Kind::Custom(HASHTREE_KIND),
        Kind::Custom(HASHTREE_LEGACY_KIND),
    ]
}

pub fn is_hashtree_root_kind(kind: Kind) -> bool {
    kind == Kind::Custom(HASHTREE_KIND) || kind == Kind::Custom(HASHTREE_LEGACY_KIND)
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
            .custom_tag(
                SingleLetterTag::lowercase(Alphabet::L),
                HASHTREE_LABEL.to_string(),
            )
            .limit(50),
    )
}

pub fn hashtree_event_identifier(event: &Event) -> Option<String> {
    event.tags.iter().find_map(|tag| {
        let slice = tag.as_slice();
        if slice.len() >= 2 && slice[0].as_str() == "d" {
            Some(slice[1].to_string())
        } else {
            None
        }
    })
}

pub fn is_hashtree_labeled_event(event: &Event) -> bool {
    event.tags.iter().any(|tag| {
        let slice = tag.as_slice();
        slice.len() >= 2 && slice[0].as_str() == "l" && slice[1].as_str() == HASHTREE_LABEL
    })
}

pub fn pick_latest_event<'a, I>(events: I) -> Option<&'a Event>
where
    I: IntoIterator<Item = &'a Event>,
{
    events.into_iter().max_by(|a, b| {
        let ordering = a.created_at.cmp(&b.created_at);
        if ordering == std::cmp::Ordering::Equal {
            a.id.cmp(&b.id)
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
    if hashtree_event_identifier(event).as_deref() != Some(tree_name)
        || !is_hashtree_labeled_event(event)
    {
        return None;
    }

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
            "hash" => hash_tag = Some(slice[1].to_string()),
            "key" => key = Some(slice[1].to_string()),
            "encryptedKey" => encrypted_key = Some(slice[1].to_string()),
            "selfEncryptedKey" => self_encrypted_key = Some(slice[1].to_string()),
            _ => {}
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::nostr::{EventBuilder, Keys, Tag, Timestamp};

    #[test]
    fn root_event_from_peer_extracts_tags() {
        let keys = Keys::generate();
        let hash = "ab".repeat(32);
        let event = EventBuilder::new(Kind::Custom(HASHTREE_KIND), "")
            .tags([
                Tag::parse(["d", "repo"]).expect("d tag"),
                Tag::parse(["l", HASHTREE_LABEL]).expect("label tag"),
                Tag::parse(vec!["hash".to_string(), hash.clone()]).expect("hash tag"),
                Tag::parse(vec!["encryptedKey".to_string(), "11".repeat(32)])
                    .expect("encryptedKey tag"),
            ])
            .sign_with_keys(&keys)
            .expect("event");

        let parsed = root_event_from_peer(&event, "peer-a", "repo").expect("root event");
        let expected_encrypted = "11".repeat(32);
        assert_eq!(parsed.hash, hash);
        assert_eq!(parsed.peer_id, "peer-a");
        assert_eq!(
            parsed.encrypted_key.as_deref(),
            Some(expected_encrypted.as_str())
        );
        assert!(parsed.key.is_none());
    }

    #[test]
    fn pick_latest_event_prefers_higher_event_id_on_timestamp_tie() {
        let keys = Keys::generate();
        let created_at = Timestamp::from_secs(1_700_000_000);
        let event_a = EventBuilder::new(Kind::Custom(HASHTREE_KIND), "")
            .custom_created_at(created_at)
            .sign_with_keys(&keys)
            .expect("event a");
        let event_b = EventBuilder::new(Kind::Custom(HASHTREE_KIND), "")
            .custom_created_at(created_at)
            .sign_with_keys(&keys)
            .expect("event b");

        let expected = if event_a.id > event_b.id {
            event_a.id
        } else {
            event_b.id
        };
        let picked = pick_latest_event([&event_a, &event_b]).expect("picked event");
        assert_eq!(picked.id, expected);
    }
}
