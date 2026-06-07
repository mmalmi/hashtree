use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

use hashtree_cli::config::{ensure_keys_string, parse_npub};
use hashtree_cli::Config;

fn load_hex_list(path: &Path) -> Result<Vec<String>> {
    if path.exists() {
        let data = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&data).unwrap_or_default())
    } else {
        Ok(Vec::new())
    }
}

fn save_hex_list(path: &Path, list: &[String]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(list)?)?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MuteEntry {
    pub(crate) pubkey: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum MuteUpdate {
    Added,
    Updated,
    Removed,
    Unchanged,
}

fn normalize_reason(reason: Option<&str>) -> Option<String> {
    reason
        .map(|r| r.trim())
        .filter(|r| !r.is_empty())
        .map(|r| r.to_string())
}

pub(crate) fn load_mute_entries(path: &Path) -> Result<Vec<MuteEntry>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let data = std::fs::read_to_string(path)?;
    let value: serde_json::Value = match serde_json::from_str(&data) {
        Ok(v) => v,
        Err(_) => return Ok(Vec::new()),
    };

    let Some(items) = value.as_array() else {
        return Ok(Vec::new());
    };

    let mut entries = Vec::new();
    for item in items {
        match item {
            serde_json::Value::String(pk) => entries.push(MuteEntry {
                pubkey: pk.to_string(),
                reason: None,
            }),
            serde_json::Value::Object(obj) => {
                let pubkey = obj.get("pubkey").and_then(|v| v.as_str());
                if let Some(pubkey) = pubkey {
                    let reason = obj.get("reason").and_then(|v| v.as_str());
                    entries.push(MuteEntry {
                        pubkey: pubkey.to_string(),
                        reason: normalize_reason(reason),
                    });
                }
            }
            _ => {}
        }
    }

    Ok(entries)
}

fn save_mute_entries(path: &Path, entries: &[MuteEntry]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let data = if entries.iter().all(|entry| entry.reason.is_none()) {
        let pubkeys: Vec<String> = entries.iter().map(|entry| entry.pubkey.clone()).collect();
        serde_json::to_string_pretty(&pubkeys)?
    } else {
        serde_json::to_string_pretty(entries)?
    };

    std::fs::write(path, data)?;
    Ok(())
}

pub(crate) fn update_mute_list_file_with_status(
    path: &Path,
    target_hex: &str,
    reason: Option<&str>,
    add: bool,
) -> Result<(Vec<MuteEntry>, MuteUpdate)> {
    let mut entries = load_mute_entries(path)?;
    let normalized_reason = normalize_reason(reason);

    let update = if add {
        if let Some(entry) = entries.iter_mut().find(|entry| entry.pubkey == target_hex) {
            if let Some(new_reason) = normalized_reason {
                if entry.reason.as_deref() != Some(new_reason.as_str()) {
                    entry.reason = Some(new_reason);
                    MuteUpdate::Updated
                } else {
                    MuteUpdate::Unchanged
                }
            } else {
                MuteUpdate::Unchanged
            }
        } else {
            entries.push(MuteEntry {
                pubkey: target_hex.to_string(),
                reason: normalized_reason,
            });
            MuteUpdate::Added
        }
    } else if let Some(pos) = entries.iter().position(|entry| entry.pubkey == target_hex) {
        entries.remove(pos);
        MuteUpdate::Removed
    } else {
        MuteUpdate::Unchanged
    };

    if matches!(
        update,
        MuteUpdate::Added | MuteUpdate::Updated | MuteUpdate::Removed
    ) {
        save_mute_entries(path, &entries)?;
    }

    Ok((entries, update))
}

pub(crate) fn update_hex_list_file_with_status(
    path: &Path,
    target_hex: &str,
    add: bool,
) -> Result<(Vec<String>, bool)> {
    let mut list = load_hex_list(path)?;
    let changed = if add {
        if list.contains(&target_hex.to_string()) {
            false
        } else {
            list.push(target_hex.to_string());
            true
        }
    } else if let Some(pos) = list.iter().position(|x| x == target_hex) {
        list.remove(pos);
        true
    } else {
        false
    };

    if changed {
        save_hex_list(path, &list)?;
    }

    Ok((list, changed))
}

#[cfg(test)]
pub(crate) fn update_hex_list_file(
    path: &Path,
    target_hex: &str,
    add: bool,
) -> Result<Vec<String>> {
    let (list, _changed) = update_hex_list_file_with_status(path, target_hex, add)?;
    Ok(list)
}

fn build_pubkey_list_event(
    kind: nostr::Kind,
    pubkeys: &[String],
    keys: &nostr::Keys,
) -> Result<nostr::Event> {
    use nostr::{EventBuilder, PublicKey, Tag};

    let tags: Vec<Tag> = pubkeys
        .iter()
        .filter_map(|pk_hex| PublicKey::from_hex(pk_hex).ok().map(Tag::public_key))
        .collect();

    EventBuilder::new(kind, "")
        .tags(tags)
        .sign_with_keys(keys)
        .context("Failed to sign list event")
}

pub(crate) fn build_mute_list_event(
    mutes: &[MuteEntry],
    keys: &nostr::Keys,
) -> Result<nostr::Event> {
    use nostr::{EventBuilder, PublicKey, Tag};

    let mut tags: Vec<Tag> = Vec::new();
    for entry in mutes {
        let Ok(pubkey) = PublicKey::from_hex(&entry.pubkey) else {
            continue;
        };

        if let Some(reason) = entry.reason.as_ref().filter(|r| !r.is_empty()) {
            tags.push(Tag::parse(vec![
                "p".to_string(),
                pubkey.to_hex(),
                reason.to_string(),
            ])?);
        } else {
            tags.push(Tag::public_key(pubkey));
        }
    }

    EventBuilder::new(nostr::Kind::Custom(10000), "")
        .tags(tags)
        .sign_with_keys(keys)
        .context("Failed to sign mute list event")
}

async fn publish_event_to_relays(relays: &[String], event_json: &str) -> usize {
    use futures::sink::SinkExt;
    use tokio_tungstenite::connect_async;

    let mut success_count = 0;
    for relay in relays {
        if let Ok((mut ws, _)) = connect_async(relay).await {
            if ws
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    event_json.to_string(),
                ))
                .await
                .is_ok()
            {
                success_count += 1;
            }
            let _ = ws.close(None).await;
        }
    }
    success_count
}

/// Follow or unfollow a user by publishing an updated kind 3 contact list.
pub(crate) async fn follow_user(data_dir: &Path, npub_str: &str, follow: bool) -> Result<()> {
    use nostr::{ClientMessage, JsonUtil, Keys, Kind};

    // Load config for relay list
    let config = Config::load()?;

    // Ensure nsec exists
    let (nsec_str, _) = ensure_keys_string()?;
    let keys = Keys::parse(&nsec_str).context("Failed to parse nsec")?;

    // Parse target npub
    let target_pubkey = parse_npub(npub_str).context("Invalid npub")?;
    let target_pubkey_hex = hex::encode(target_pubkey);

    // Update contact list locally
    let contacts_file = data_dir.join("contacts.json");
    let (contacts, changed) =
        update_hex_list_file_with_status(&contacts_file, &target_pubkey_hex, follow)?;

    if follow {
        if changed {
            println!("Following: {}", npub_str);
        } else {
            println!("Already following: {}", npub_str);
            return Ok(());
        }
    } else if changed {
        println!("Unfollowed: {}", npub_str);
    } else {
        println!("Not following: {}", npub_str);
        return Ok(());
    }

    let event = build_pubkey_list_event(Kind::ContactList, &contacts, &keys)?;

    let event_json = ClientMessage::event(event).as_json();

    let success_count = publish_event_to_relays(&config.nostr.relays, &event_json).await;

    println!("Published contact list to {} relays", success_count);
    Ok(())
}

/// Mute or unmute a user by publishing an updated kind 10000 mute list.
pub(crate) async fn mute_user(
    data_dir: &Path,
    npub_str: &str,
    reason: Option<&str>,
    mute: bool,
) -> Result<()> {
    use nostr::{ClientMessage, JsonUtil, Keys};

    let config = Config::load()?;

    let (nsec_str, _) = ensure_keys_string()?;
    let keys = Keys::parse(&nsec_str).context("Failed to parse nsec")?;

    let target_pubkey = parse_npub(npub_str).context("Invalid npub")?;
    let target_pubkey_hex = hex::encode(target_pubkey);

    let mutes_file = data_dir.join("mutes.json");
    let (mutes, update) =
        update_mute_list_file_with_status(&mutes_file, &target_pubkey_hex, reason, mute)?;

    if mute {
        if update == MuteUpdate::Added {
            println!("Muted: {}", npub_str);
        } else if update == MuteUpdate::Updated {
            println!("Updated mute reason for: {}", npub_str);
        } else {
            println!("Already muted: {}", npub_str);
            return Ok(());
        }
    } else if update == MuteUpdate::Removed {
        println!("Unmuted: {}", npub_str);
    } else {
        println!("Not muted: {}", npub_str);
        return Ok(());
    }

    let event = build_mute_list_event(&mutes, &keys)?;
    let event_json = ClientMessage::event(event).as_json();

    let success_count = publish_event_to_relays(&config.nostr.relays, &event_json).await;

    println!("Published mute list to {} relays", success_count);
    Ok(())
}

/// Show or update Nostr profile (kind 0).
pub(crate) async fn update_profile(
    name: Option<String>,
    about: Option<String>,
    picture: Option<String>,
) -> Result<()> {
    use nostr::nips::nip19::ToBech32;
    use nostr::{EventBuilder, Filter, Keys, Kind};
    use nostr_sdk::ClientBuilder;
    use std::time::Duration;

    // Load config for relay list
    let config = Config::load()?;

    // Ensure nsec exists
    let (nsec_str, _) = ensure_keys_string()?;
    let keys = Keys::parse(&nsec_str).context("Failed to parse nsec")?;
    let npub = keys.public_key().to_bech32()?;

    // Check if we're just showing the profile (no args)
    let is_show_only = name.is_none() && about.is_none() && picture.is_none();

    // Fetch existing profile
    let client = ClientBuilder::default().build();
    for relay in &config.nostr.relays {
        let _ = client.add_relay(relay).await;
    }
    client.connect().await;

    // Wait for relay connections to establish
    tokio::time::sleep(Duration::from_millis(500)).await;

    let filter = Filter::new()
        .author(keys.public_key())
        .kind(Kind::Metadata)
        .limit(1);

    let timeout = Duration::from_secs(5);
    let events = tokio::time::timeout(timeout, client.fetch_events(filter, timeout))
        .await
        .ok()
        .and_then(|r| r.ok())
        .map(|events| events.to_vec())
        .unwrap_or_default();
    let _ = client.disconnect().await;

    // Parse existing profile or start fresh
    let mut profile: serde_json::Map<String, serde_json::Value> = events
        .into_iter()
        .next()
        .and_then(|e| serde_json::from_str(&e.content).ok())
        .unwrap_or_default();

    if is_show_only {
        // Just display current profile
        println!("Profile: {}\n", npub);
        if let Some(n) = profile.get("name").and_then(|v| v.as_str()) {
            println!("  name:    {}", n);
        }
        if let Some(a) = profile.get("about").and_then(|v| v.as_str()) {
            println!("  about:   {}", a);
        }
        if let Some(p) = profile.get("picture").and_then(|v| v.as_str()) {
            println!("  picture: {}", p);
        }
        if profile.is_empty() {
            println!("  (no profile set)");
        }
        return Ok(());
    }

    // Update fields
    if let Some(n) = name {
        profile.insert("name".to_string(), serde_json::Value::String(n));
    }
    if let Some(a) = about {
        profile.insert("about".to_string(), serde_json::Value::String(a));
    }
    if let Some(p) = picture {
        profile.insert("picture".to_string(), serde_json::Value::String(p));
    }

    // Build and sign kind 0 event
    let content = serde_json::to_string(&profile)?;
    let event = EventBuilder::new(Kind::Metadata, &content)
        .sign_with_keys(&keys)
        .context("Failed to sign profile event")?;

    // Reuse the client we already have connected for publishing
    let client = ClientBuilder::default().build();
    for relay in &config.nostr.relays {
        let _ = client.add_relay(relay).await;
    }
    client.connect().await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Publish using nostr_sdk
    match client.send_event(&event).await {
        Ok(output) => {
            let success_count = output.success.len();
            let failed_count = output.failed.len();
            if success_count > 0 {
                println!("Profile updated, published to {} relays", success_count);
            }
            if failed_count > 0 {
                eprintln!("Failed to publish to {} relays", failed_count);
            }
        }
        Err(e) => {
            eprintln!("Failed to publish profile: {}", e);
        }
    }

    let _ = client.disconnect().await;
    Ok(())
}

/// List users we follow.
pub(crate) async fn list_following(data_dir: &Path) -> Result<()> {
    use nostr::nips::nip19::ToBech32;
    use nostr::PublicKey;

    // Load contacts from local storage
    let contacts_file = data_dir.join("contacts.json");
    let contacts = load_hex_list(&contacts_file)?;

    if contacts.is_empty() {
        println!("Not following anyone");
        return Ok(());
    }

    println!("Following {} users:", contacts.len());
    for pk_hex in &contacts {
        if let Ok(pk) = PublicKey::from_hex(pk_hex) {
            println!("  {}", pk.to_bech32().expect("npub encoding is infallible"));
        } else {
            println!("  {} (invalid)", pk_hex);
        }
    }

    Ok(())
}

/// List users we mute.
pub(crate) async fn list_muted(data_dir: &Path) -> Result<()> {
    use nostr::nips::nip19::ToBech32;
    use nostr::PublicKey;

    let mutes_file = data_dir.join("mutes.json");
    let mutes = load_mute_entries(&mutes_file)?;

    if mutes.is_empty() {
        println!("Not muting anyone");
        return Ok(());
    }

    println!("Muted {} users:", mutes.len());
    for entry in &mutes {
        let label = if let Ok(pk) = PublicKey::from_hex(&entry.pubkey) {
            pk.to_bech32().expect("npub encoding is infallible")
        } else {
            format!("{} (invalid)", entry.pubkey)
        };
        if let Some(reason) = entry.reason.as_ref() {
            println!("  {} - {}", label, reason);
        } else {
            println!("  {}", label);
        }
    }

    Ok(())
}
