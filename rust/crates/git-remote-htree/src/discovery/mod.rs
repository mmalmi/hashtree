//! Shared, read-only discovery of public named trees for Git and the CLI.
mod local;
mod observe;
mod selection;
mod state;

pub use observe::{search, search_with, SearchOptions};
pub use selection::{choose, suggestions};
pub use state::{Candidate, SearchResults};

use crate::nostr_client::{load_keys, resolve_identity};
use anyhow::{ensure, Result};
use nostr::{PublicKey, ToBech32};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub owner: Option<String>,
    pub name: String,
    pub path: Option<String>,
}

impl Query {
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.strip_prefix("htree://").unwrap_or(input);
        ensure!(
            !input.is_empty() && input.len() <= 512,
            "Enter a tree name or publisher/tree"
        );
        ensure!(!input.chars().any(char::is_control) && !input.contains(['#', '?', ':']), "Name searches cannot contain control characters, URL fragments or queries; use an explicit npub URL for shared/private trees");
        let parts: Vec<_> = input.splitn(3, '/').collect();
        ensure!(
            parts.iter().all(|part| !part.is_empty()),
            "Enter a tree name or publisher/tree"
        );
        Ok(if parts.len() == 1 {
            Self {
                owner: None,
                name: parts[0].into(),
                path: None,
            }
        } else {
            Self {
                owner: Some(parts[0].into()),
                name: parts[1].into(),
                path: parts.get(2).map(|s| s.to_string()),
            }
        })
    }
}

/// Local aliases are explicit user choices. Profile names are only search hints.
/// Malformed explicit identities must not silently become fuzzy searches.
pub fn is_name_query(input: &str) -> bool {
    let raw = input.strip_prefix("htree://").unwrap_or(input);
    let owner = raw.split('/').next().unwrap_or(raw);
    if owner == "self"
        || owner.starts_with("npub1")
        || owner.starts_with("nhash1")
        || owner.len() == 64
        || hashtree_core::Cid::parse(owner).is_ok()
    {
        return false;
    }
    !(raw.contains('/')
        && load_keys()
            .iter()
            .any(|key| key.petname.as_deref() == Some(owner)))
}

/// Resolve explicit inputs without any network discovery.
pub fn explicit_target(input: &str) -> Result<Option<String>> {
    let raw = input.strip_prefix("htree://").unwrap_or(input);
    let owner = raw.split('/').next().unwrap_or(raw);
    if owner.starts_with("nhash1") || hashtree_core::Cid::parse(owner).is_ok() {
        return Ok(Some(input.to_string()));
    }
    if is_name_query(input) {
        return Ok(None);
    }
    let (_, rest) = raw
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("An explicit publisher needs a tree name"))?;
    ensure!(!rest.is_empty(), "An explicit publisher needs a tree name");
    let (pubkey, _) = resolve_identity(owner)?;
    Ok(Some(format!(
        "htree://{}/{}",
        PublicKey::from_hex(&pubkey)?.to_bech32()?,
        rest
    )))
}

#[cfg(test)]
mod tests;
