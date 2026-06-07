use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use nostr::{Event, EventBuilder, Keys, Kind, PublicKey, Tag, Timestamp};
use serde::Deserialize;

use super::SocialGraphBackend;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalListFileState {
    pub contacts_modified: Option<SystemTime>,
    pub mutes_modified: Option<SystemTime>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LocalListSyncOutcome {
    pub contacts_changed: bool,
    pub mutes_changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct StoredMuteEntry {
    pubkey: String,
    #[serde(default)]
    reason: Option<String>,
}

pub fn read_local_list_file_state(data_dir: &Path) -> Result<LocalListFileState> {
    Ok(LocalListFileState {
        contacts_modified: file_modified(data_dir.join("contacts.json"))?,
        mutes_modified: file_modified(data_dir.join("mutes.json"))?,
    })
}

pub fn sync_local_list_files_force(
    backend: &(impl SocialGraphBackend + ?Sized),
    data_dir: &Path,
    keys: &Keys,
) -> Result<LocalListFileState> {
    let state = read_local_list_file_state(data_dir)?;
    let _ = sync_local_list_files_between(
        backend,
        data_dir,
        keys,
        &LocalListFileState::default(),
        &state,
        true,
    )?;
    Ok(state)
}

pub fn sync_local_list_files_if_changed(
    backend: &(impl SocialGraphBackend + ?Sized),
    data_dir: &Path,
    keys: &Keys,
    previous_state: &mut LocalListFileState,
) -> Result<LocalListSyncOutcome> {
    let current_state = read_local_list_file_state(data_dir)?;
    let outcome = sync_local_list_files_between(
        backend,
        data_dir,
        keys,
        previous_state,
        &current_state,
        false,
    )?;
    *previous_state = current_state;
    Ok(outcome)
}

fn sync_local_list_files_between(
    backend: &(impl SocialGraphBackend + ?Sized),
    data_dir: &Path,
    keys: &Keys,
    previous_state: &LocalListFileState,
    current_state: &LocalListFileState,
    force_existing: bool,
) -> Result<LocalListSyncOutcome> {
    let contacts_changed = should_sync_list(
        previous_state.contacts_modified,
        current_state.contacts_modified,
        force_existing,
    );
    if contacts_changed {
        let event = build_contact_list_event(
            &load_contacts(data_dir.join("contacts.json"))?,
            keys,
            list_timestamp(
                current_state.contacts_modified,
                previous_state.contacts_modified,
            ),
        )?;
        backend.ingest_event(&event)?;
    }

    let mutes_changed = should_sync_list(
        previous_state.mutes_modified,
        current_state.mutes_modified,
        force_existing,
    );
    if mutes_changed {
        let event = build_mute_list_event(
            &load_mutes(data_dir.join("mutes.json"))?,
            keys,
            list_timestamp(current_state.mutes_modified, previous_state.mutes_modified),
        )?;
        backend.ingest_event(&event)?;
    }

    Ok(LocalListSyncOutcome {
        contacts_changed,
        mutes_changed,
    })
}

fn should_sync_list(
    previous: Option<SystemTime>,
    current: Option<SystemTime>,
    force_existing: bool,
) -> bool {
    if previous != current {
        return previous.is_some() || current.is_some();
    }

    force_existing && current.is_some()
}

fn file_modified(path: impl AsRef<Path>) -> Result<Option<SystemTime>> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(None);
    }
    let metadata = fs::metadata(path)?;
    Ok(metadata.modified().ok())
}

fn list_timestamp(current: Option<SystemTime>, previous: Option<SystemTime>) -> Timestamp {
    if let Some(current) = current {
        return Timestamp::from_secs(system_time_secs(current));
    }
    if previous.is_some() {
        return Timestamp::now();
    }
    Timestamp::now()
}

fn system_time_secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn load_contacts(path: impl AsRef<Path>) -> Result<Vec<String>> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let data = fs::read_to_string(path)?;
    let contacts = serde_json::from_str::<Vec<String>>(&data).unwrap_or_default();
    Ok(contacts)
}

fn load_mutes(path: impl AsRef<Path>) -> Result<Vec<StoredMuteEntry>> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(Vec::new());
    }

    let data = fs::read_to_string(path)?;
    let value: serde_json::Value = serde_json::from_str(&data).unwrap_or_default();
    let Some(items) = value.as_array() else {
        return Ok(Vec::new());
    };

    let mut entries = Vec::new();
    for item in items {
        match item {
            serde_json::Value::String(pubkey) => entries.push(StoredMuteEntry {
                pubkey: pubkey.clone(),
                reason: None,
            }),
            serde_json::Value::Object(_) => {
                if let Ok(entry) = serde_json::from_value::<StoredMuteEntry>(item.clone()) {
                    entries.push(entry);
                }
            }
            _ => {}
        }
    }

    Ok(entries)
}

fn build_contact_list_event(
    pubkeys: &[String],
    keys: &Keys,
    created_at: Timestamp,
) -> Result<Event> {
    let tags = pubkeys
        .iter()
        .filter_map(|pubkey| PublicKey::from_hex(pubkey).ok())
        .map(Tag::public_key)
        .collect::<Vec<_>>();
    EventBuilder::new(Kind::ContactList, "")
        .tags(tags)
        .custom_created_at(created_at)
        .sign_with_keys(keys)
        .context("sign local contact list event")
}

fn build_mute_list_event(
    entries: &[StoredMuteEntry],
    keys: &Keys,
    created_at: Timestamp,
) -> Result<Event> {
    let mut tags = Vec::new();
    for entry in entries {
        let Ok(pubkey) = PublicKey::from_hex(&entry.pubkey) else {
            continue;
        };
        if let Some(reason) = entry
            .reason
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            tags.push(Tag::parse(vec![
                "p".to_string(),
                pubkey.to_hex(),
                reason.to_string(),
            ])?);
        } else {
            tags.push(Tag::public_key(pubkey));
        }
    }
    EventBuilder::new(Kind::MuteList, "")
        .tags(tags)
        .custom_created_at(created_at)
        .sign_with_keys(keys)
        .context("sign local mute list event")
}
