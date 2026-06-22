use anyhow::{Context, Result};
use git_remote_htree::nostr_client::{hashtree_root_kinds, resolve_identity};
use nostr::{Alphabet, Event, EventId, Filter, PublicKey, SingleLetterTag, Timestamp, ToBech32};
use nostr_sdk::Client;
use std::collections::HashMap;
use std::time::Duration;

const LABEL_HASHTREE: &str = "hashtree";
const LABEL_GIT: &str = "git";

pub(crate) async fn list_repos(owner: Option<&str>) -> Result<()> {
    let owner = owner
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("self");

    let (pubkey, _secret_key) = resolve_identity(owner)?;
    let config = hashtree_config::Config::load_or_default();
    let author = PublicKey::from_hex(&pubkey).context("Invalid owner pubkey")?;
    let owner_display = author.to_bech32().unwrap_or(pubkey);

    let relays = hashtree_config::resolve_relays(
        &config.nostr.relays,
        Some(config.server.bind_address.as_str()),
    );
    let repos = fetch_git_repos(author, &relays).await?;
    if repos.is_empty() {
        println!("No git repos found for {}.", owner_display);
        return Ok(());
    }

    println!("Git repos for {}:", owner_display);
    for repo_name in repos {
        println!("  htree://{}/{}", owner_display, repo_name);
    }

    Ok(())
}

async fn fetch_git_repos(author: PublicKey, relays: &[String]) -> Result<Vec<String>> {
    let client = Client::default();

    for relay in relays {
        if let Err(err) = client.add_relay(relay).await {
            tracing::warn!("Failed to add relay {}: {}", relay, err);
        }
    }
    client.connect().await;

    wait_for_connected_relay(&client).await?;

    let filter = Filter::new()
        .kinds(hashtree_root_kinds())
        .author(author)
        .custom_tag(SingleLetterTag::lowercase(Alphabet::L), LABEL_GIT)
        .limit(500);

    let events = match tokio::time::timeout(
        Duration::from_secs(3),
        client.fetch_events(filter, Duration::from_secs(3)),
    )
    .await
    {
        Ok(Ok(events)) => events.to_vec(),
        Ok(Err(err)) => {
            let _ = client.disconnect().await;
            return Err(anyhow::anyhow!(
                "Failed to fetch git repo events from relays: {}",
                err
            ));
        }
        Err(_) => {
            let _ = client.disconnect().await;
            return Err(anyhow::anyhow!(
                "Timed out fetching git repo events from relays"
            ));
        }
    };

    let _ = client.disconnect().await;

    let mut latest_by_repo: HashMap<String, (Timestamp, EventId)> = HashMap::new();
    for event in events {
        let Some(repo_name) = git_repo_name(&event) else {
            continue;
        };

        let entry = latest_by_repo
            .entry(repo_name.to_string())
            .or_insert((event.created_at, event.id));
        if (event.created_at, event.id) > (entry.0, entry.1) {
            *entry = (event.created_at, event.id);
        }
    }

    let mut repos: Vec<String> = latest_by_repo.into_keys().collect();
    repos.sort();
    Ok(repos)
}

async fn wait_for_connected_relay(client: &Client) -> Result<()> {
    let start = std::time::Instant::now();
    loop {
        let relays = client.relays().await;
        for relay in relays.values() {
            if relay.is_connected() {
                return Ok(());
            }
        }
        if start.elapsed() > Duration::from_secs(2) {
            let _ = client.disconnect().await;
            return Err(anyhow::anyhow!(
                "Failed to connect to any relay while listing repos"
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
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
