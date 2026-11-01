use anyhow::{ensure, Context, Result};
use nostr::PublicKey;
use std::path::Path;

/// Read the CLI's existing local lists without opening or modifying daemon stores.
pub(super) fn list(path: &Path) -> Result<Option<Vec<PublicKey>>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Cannot read local social list"),
    };
    let value: serde_json::Value = serde_json::from_reader(std::io::Read::take(file, 1024 * 1024))
        .context("Invalid local social list")?;
    let items = value
        .as_array()
        .context("Local social list must be an array")?;
    ensure!(
        items.len() <= 10_000,
        "Local social list is too large for discovery"
    );
    items
        .iter()
        .map(|item| {
            let key = item
                .as_str()
                .or_else(|| item.get("pubkey").and_then(|v| v.as_str()))
                .context("Invalid public key in local social list")?;
            Ok(PublicKey::parse(key)?)
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}
