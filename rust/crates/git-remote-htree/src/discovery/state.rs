use super::Query;
use hashtree_core::TreeVisibility;
use hashtree_nostr::parse_verified_hashtree_root_event;
use nostr::{Event, JsonUtil, Kind, PublicKey, Timestamp, ToBech32};
use std::collections::{BTreeMap, BTreeSet};

pub(super) const MAX_AUTHORS: usize = 256;
const MAX_EVENTS: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub url: String,
    pub name: String,
    pub publisher: String,
    pub distance: u32,
}

#[derive(Debug, Default)]
pub struct SearchResults {
    pub candidates: Vec<Candidate>,
    pub truncated: bool,
}

pub(super) struct State {
    pub roots: BTreeSet<PublicKey>,
    pub events: BTreeMap<(PublicKey, Kind, String), Event>,
    pub truncated: bool,
    pub local_follows: Option<BTreeSet<PublicKey>>,
    pub local_mutes: BTreeSet<PublicKey>,
}

fn tag<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    event.tags.iter().find_map(|t| {
        let t = t.as_slice();
        (t.first().map(String::as_str) == Some(name))
            .then(|| t.get(1).map(String::as_str))
            .flatten()
    })
}

impl State {
    pub fn new(roots: impl IntoIterator<Item = PublicKey>) -> Self {
        Self {
            roots: roots.into_iter().collect(),
            events: BTreeMap::new(),
            truncated: false,
            local_follows: None,
            local_mutes: BTreeSet::new(),
        }
    }

    pub fn ingest(&mut self, event: Event) {
        if event.content.len() > 32_768
            || event.tags.len() > 2048
            || event.as_json().len() > 32_768
            || event.created_at > Timestamp::from(Timestamp::now().as_secs() + 300)
            || event.verify().is_err()
        {
            return;
        }
        let id = if crate::nostr_client::is_hashtree_root_kind(event.kind) {
            tag(&event, "d").unwrap_or("").to_string()
        } else if matches!(
            event.kind,
            Kind::Metadata | Kind::ContactList | Kind::MuteList
        ) {
            String::new()
        } else {
            return;
        };
        let key = (event.pubkey, event.kind, id);
        if self.events.len() >= MAX_EVENTS && !self.events.contains_key(&key) {
            self.truncated = true;
            return;
        }
        if self.events.get(&key).is_none_or(|old| {
            event.created_at > old.created_at
                || (event.created_at == old.created_at && event.id < old.id)
        }) {
            self.events.insert(key, event);
        }
    }

    fn people(&self, author: PublicKey, kind: Kind) -> BTreeSet<PublicKey> {
        if kind == Kind::ContactList && self.roots.contains(&author) {
            if let Some(follows) = &self.local_follows {
                return follows.clone();
            }
        }
        self.events
            .get(&(author, kind, String::new()))
            .into_iter()
            .flat_map(|e| e.tags.iter())
            .filter_map(|t| {
                let t = t.as_slice();
                if t.first().map(String::as_str) != Some("p") {
                    return None;
                }
                PublicKey::from_hex(t.get(1)?).ok()
            })
            .collect()
    }

    pub fn distances(&self) -> BTreeMap<PublicKey, u32> {
        let muted: BTreeSet<_> = self
            .roots
            .iter()
            .flat_map(|pk| self.people(*pk, Kind::MuteList))
            .chain(self.local_mutes.iter().copied())
            .collect();
        let mut distances: BTreeMap<_, _> = self.roots.iter().map(|pk| (*pk, 0)).collect();
        for depth in 0..2 {
            let next: BTreeSet<_> = distances
                .iter()
                .filter(|(_, d)| **d == depth)
                .flat_map(|(pk, _)| self.people(*pk, Kind::ContactList))
                .collect();
            for pk in next {
                if distances.len() > MAX_AUTHORS {
                    break;
                }
                if !muted.contains(&pk) {
                    distances.entry(pk).or_insert(depth + 1);
                }
            }
        }
        distances.retain(|pk, _| !muted.contains(pk));
        distances
    }

    fn profile(&self, pk: PublicKey) -> (String, String) {
        let metadata = self
            .events
            .get(&(pk, Kind::Metadata, String::new()))
            .and_then(|e| serde_json::from_str::<serde_json::Value>(&e.content).ok());
        let field = |name| {
            metadata
                .as_ref()
                .and_then(|v| v.get(name))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        (field("name"), field("display_name"))
    }

    pub fn results(&self, query: &Query, git: bool) -> SearchResults {
        let distances = self.distances();
        let mut candidates = BTreeMap::new();
        for event in self.events.values() {
            if !distances.contains_key(&event.pubkey) {
                continue;
            }
            // Choose the newest head before checking visibility, labels or root validity.
            // A newer private/invalid head must not resurrect an old public suggestion.
            if !crate::nostr_client::is_hashtree_root_kind(event.kind) {
                continue;
            }
            let Some(name) = tag(event, "d") else {
                continue;
            };
            if !name.to_lowercase().contains(&query.name.to_lowercase()) {
                continue;
            }
            let key = (event.pubkey, name.to_string());
            let old: Option<&&Event> = candidates.get(&key);
            if old.is_none_or(|old| {
                event.created_at > old.created_at
                    || (event.created_at == old.created_at && event.id < old.id)
            }) {
                candidates.insert(key, event);
            }
        }
        let mut found = Vec::new();
        for ((pk, name), event) in candidates {
            let Ok(Some(root)) = parse_verified_hashtree_root_event(event) else {
                continue;
            };
            if root.visibility != TreeVisibility::Public
                || root.encrypted_key.is_some()
                || root.self_encrypted_key.is_some()
                || root.self_encrypted_link_key.is_some()
                || (git && !root.labels.iter().any(|l| l == "git"))
            {
                continue;
            }
            // Do not let remote names inject terminal output, shell commands or URL syntax.
            if !name.split('/').all(|part| {
                !part.is_empty()
                    && part != "."
                    && part != ".."
                    && part
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
            }) {
                continue;
            }
            let (username, display) = self.profile(pk);
            if let Some(owner) = &query.owner {
                let owner = owner.to_lowercase();
                if PublicKey::parse(&owner).ok() != Some(pk)
                    && !username.to_lowercase().contains(&owner)
                    && !display.to_lowercase().contains(&owner)
                {
                    continue;
                }
            }
            let publisher = if username.is_empty() {
                display
            } else {
                username
            };
            let publisher: String = publisher
                .chars()
                .filter(|c| {
                    !c.is_control()
                        && !matches!(*c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
                })
                .take(60)
                .collect();
            let mut url = format!("htree://{}/{}", pk.to_bech32().expect("public key"), name);
            if let Some(path) = &query.path {
                url.push('/');
                url.push_str(path);
            }
            found.push(Candidate {
                url,
                name,
                publisher,
                distance: distances[&pk],
            });
        }
        found.sort_by_key(|c| {
            (
                c.name.to_lowercase() != query.name.to_lowercase(),
                c.distance,
                c.name.clone(),
                c.url.clone(),
            )
        });
        let truncated = self.truncated || found.len() > 8 || distances.len() > MAX_AUTHORS;
        found.truncate(8);
        SearchResults {
            candidates: found,
            truncated,
        }
    }
}
