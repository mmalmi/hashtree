use anyhow::Result;
use nostr_sdk::prelude::*;
use std::collections::HashMap;
use std::time::Duration;

use super::{
    hashtree_root_kinds, Event, KIND_REPO_ANNOUNCEMENT, KIND_STATUS_APPLIED, LABEL_GIT,
    LABEL_HASHTREE,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GitRepoAnnouncement {
    pub(super) repo_name: String,
    pub(super) created_at: Timestamp,
    pub(super) event_id: EventId,
}

pub(super) fn pick_latest_event<'a, I>(events: I) -> Option<&'a Event>
where
    I: IntoIterator<Item = &'a Event>,
{
    events
        .into_iter()
        .max_by_key(|event| (event.created_at, event.id))
}

fn is_matching_repo_event(event: &Event, repo_name: &str) -> bool {
    let has_hashtree_label = event.tags.iter().any(|tag| {
        let slice = tag.as_slice();
        slice.len() >= 2 && slice[0].as_str() == "l" && slice[1].as_str() == LABEL_HASHTREE
    });

    if !has_hashtree_label {
        return false;
    }

    event.tags.iter().any(|tag| {
        let slice = tag.as_slice();
        slice.len() >= 2 && slice[0].as_str() == "d" && slice[1].as_str() == repo_name
    })
}

pub(super) fn pick_latest_repo_event<'a, I>(events: I, repo_name: &str) -> Option<&'a Event>
where
    I: IntoIterator<Item = &'a Event>,
{
    pick_latest_event(
        events
            .into_iter()
            .filter(|event| is_matching_repo_event(event, repo_name)),
    )
}

fn git_repo_name(event: &Event) -> Option<&str> {
    let has_hashtree_label = event.tags.iter().any(|tag| {
        let slice = tag.as_slice();
        slice.len() >= 2 && slice[0].as_str() == "l" && slice[1].as_str() == LABEL_HASHTREE
    });
    let has_git_label = event.tags.iter().any(|tag| {
        let slice = tag.as_slice();
        slice.len() >= 2 && slice[0].as_str() == "l" && slice[1].as_str() == LABEL_GIT
    });
    if !has_hashtree_label || !has_git_label {
        return None;
    }

    event.tags.iter().find_map(|tag| {
        let slice = tag.as_slice();
        if slice.len() < 2 || slice[0].as_str() != "d" {
            return None;
        }
        let repo_name = slice[1].as_str();
        if repo_name.is_empty() {
            None
        } else {
            Some(repo_name)
        }
    })
}

pub(super) fn list_git_repo_announcements(events: &[Event]) -> Vec<GitRepoAnnouncement> {
    let mut latest_by_repo: HashMap<String, (Timestamp, EventId)> = HashMap::new();

    for event in events {
        let Some(repo_name) = git_repo_name(event) else {
            continue;
        };

        let entry = latest_by_repo
            .entry(repo_name.to_string())
            .or_insert((event.created_at, event.id));
        if (event.created_at, event.id) > (entry.0, entry.1) {
            *entry = (event.created_at, event.id);
        }
    }

    let mut repos: Vec<GitRepoAnnouncement> = latest_by_repo
        .into_iter()
        .map(|(repo_name, (created_at, event_id))| GitRepoAnnouncement {
            repo_name,
            created_at,
            event_id,
        })
        .collect();
    repos.sort_by(|left, right| left.repo_name.cmp(&right.repo_name));
    repos
}

pub(super) fn build_git_repo_list_filter(author: PublicKey) -> Filter {
    Filter::new()
        .kinds(hashtree_root_kinds())
        .author(author)
        .custom_tag(SingleLetterTag::lowercase(Alphabet::L), LABEL_GIT)
        .limit(500)
}

pub(super) fn build_repo_event_filter(author: PublicKey, repo_name: &str) -> Filter {
    Filter::new()
        .kinds(hashtree_root_kinds())
        .author(author)
        .custom_tag(SingleLetterTag::lowercase(Alphabet::D), repo_name)
        .custom_tag(SingleLetterTag::lowercase(Alphabet::L), LABEL_HASHTREE)
        .limit(50)
}

pub(super) fn build_repo_announcement_filter(author: PublicKey, repo_name: &str) -> Filter {
    Filter::new()
        .kind(Kind::Custom(KIND_REPO_ANNOUNCEMENT))
        .author(author)
        .custom_tag(SingleLetterTag::lowercase(Alphabet::D), repo_name)
        .limit(50)
}

pub(super) fn next_replaceable_created_at(
    now: Timestamp,
    latest_existing: Option<Timestamp>,
) -> Timestamp {
    match latest_existing {
        Some(latest) if latest >= now => Timestamp::from_secs(latest.as_secs().saturating_add(1)),
        _ => now,
    }
}

pub(super) async fn latest_repo_event_created_at(
    client: &Client,
    author: PublicKey,
    repo_name: &str,
    timeout: Duration,
) -> Option<Timestamp> {
    let events = client
        .fetch_events(build_repo_event_filter(author, repo_name), timeout)
        .await
        .ok()?;
    pick_latest_repo_event(events.iter(), repo_name).map(|event| event.created_at)
}

pub(super) async fn latest_repo_announcement_created_at(
    client: &Client,
    author: PublicKey,
    repo_name: &str,
    timeout: Duration,
) -> Option<Timestamp> {
    let events = client
        .fetch_events(build_repo_announcement_filter(author, repo_name), timeout)
        .await
        .ok()?;
    pick_latest_event(events.iter()).map(|event| event.created_at)
}

pub(super) fn extract_repo_announcement_euc(event: &Event) -> Option<String> {
    event.tags.iter().find_map(|tag| {
        let slice = tag.as_slice();
        if slice.len() >= 3 && slice[0].as_str() == "r" && slice[2].as_str() == "euc" {
            let commit = slice[1].as_str();
            if commit.is_empty() {
                None
            } else {
                Some(commit.to_string())
            }
        } else {
            None
        }
    })
}

pub(super) fn append_repo_discovery_labels(tags: &mut Vec<Tag>, repo_name: &str) {
    tags.push(Tag::custom(
        TagKind::custom("l"),
        vec![LABEL_GIT.to_string()],
    ));

    let parts: Vec<&str> = repo_name.split('/').collect();
    for i in 1..parts.len() {
        let prefix = parts[..i].join("/");
        tags.push(Tag::custom(TagKind::custom("l"), vec![prefix]));
    }
}

fn relay_host(url: &str) -> Option<&str> {
    let stripped = url
        .strip_prefix("ws://")
        .or_else(|| url.strip_prefix("wss://"))
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    let authority = stripped.split('/').next().unwrap_or(stripped);
    if authority.is_empty() {
        return None;
    }

    if let Some(host) = authority.strip_prefix('[') {
        return host.split(']').next().filter(|value| !value.is_empty());
    }

    authority
        .split(':')
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn is_local_relay_url(url: &str) -> bool {
    relay_host(url).is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host == "127.0.0.1"
            || host == "::1"
            || host.starts_with("127.")
    })
}

fn has_non_local_relay(urls: &[String]) -> bool {
    urls.iter().any(|url| !is_local_relay_url(url))
}

pub(super) fn validate_repo_publish_relays(
    configured: &[String],
    connected: &[String],
) -> Result<()> {
    if connected.is_empty() {
        anyhow::bail!(
            "No relay confirmed repo publication. Another machine will not discover this repo via htree://<npub>/... Check [nostr].relays in ~/.hashtree/config.toml."
        );
    }

    if has_non_local_relay(configured) && !has_non_local_relay(connected) {
        anyhow::bail!(
            "No public relay confirmed repo publication; local relays only: {}. Another machine will not discover this repo via htree://<npub>/... Check [nostr].relays in ~/.hashtree/config.toml.",
            connected.join(", ")
        );
    }

    Ok(())
}

pub(super) fn latest_trusted_pr_status_kinds(
    pr_events: &[Event],
    status_events: &[Event],
    repo_owner_pubkey: &str,
) -> HashMap<String, u16> {
    let pr_authors: HashMap<String, String> = pr_events
        .iter()
        .map(|event| (event.id.to_hex(), event.pubkey.to_hex()))
        .collect();

    let mut trusted_statuses: HashMap<String, Vec<&Event>> = HashMap::new();
    for status in status_events {
        let signer_pubkey = status.pubkey.to_hex();
        for tag in status.tags.iter() {
            let slice = tag.as_slice();
            if slice.len() < 2 || slice[0].as_str() != "e" {
                continue;
            }

            let pr_id = slice[1].to_string();
            let Some(pr_author_pubkey) = pr_authors.get(&pr_id) else {
                continue;
            };

            let trusted = if status.kind.as_u16() == KIND_STATUS_APPLIED {
                signer_pubkey == repo_owner_pubkey
            } else {
                signer_pubkey == *pr_author_pubkey || signer_pubkey == repo_owner_pubkey
            };
            if trusted {
                trusted_statuses.entry(pr_id).or_default().push(status);
            }
        }
    }

    let mut latest_status = HashMap::new();
    for (pr_id, events) in trusted_statuses {
        if let Some(applied) = pick_latest_event(
            events
                .iter()
                .copied()
                .filter(|event| event.kind.as_u16() == KIND_STATUS_APPLIED),
        ) {
            latest_status.insert(pr_id, applied.kind.as_u16());
        } else if let Some(latest) = pick_latest_event(events.iter().copied()) {
            latest_status.insert(pr_id, latest.kind.as_u16());
        }
    }

    latest_status
}
