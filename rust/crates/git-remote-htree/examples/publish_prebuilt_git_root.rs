use anyhow::{ensure, Context, Result};
use git_remote_htree::nostr_client::{resolve_identity, NostrClient};
use hashtree_config::Config;
use hashtree_core::{EncryptionMode, ObjectKey};
use nostr_sdk::prelude::PublicKey;
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Root {
    hash: String,
    key: String,
    refs: HashMap<String, String>,
}

impl Root {
    fn validate(&self) -> Result<ObjectKey> {
        ensure!(
            self.hash.len() == 64
                && self
                    .hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid root hash"
        );
        ensure!(
            self.refs.contains_key("HEAD") && self.refs.keys().any(|s| s.starts_with("refs/")),
            "expected full refs including HEAD and a named ref"
        );
        let key = ObjectKey::from_hex(&self.key).context("invalid public CHK")?;
        ensure!(key.mode() != EncryptionMode::RndV1, "expected a public CHK");
        Ok(key)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    owner: String,
    repository: String,
    expected: Root,
    candidate: Root,
}

fn main() -> Result<()> {
    let request: Request =
        serde_json::from_reader(std::io::stdin().lock()).context("invalid JSON on stdin")?;
    let old_key = request.expected.validate()?;
    let new_key = request.candidate.validate()?;
    let name = &request.repository;
    ensure!(
        !name.chars().any(char::is_control)
            && name
                .split('/')
                .all(|p| !p.is_empty() && p != "." && p != ".."),
        "invalid repo name"
    );
    ensure!(
        request.expected.hash != request.candidate.hash,
        "candidate is already the expected root"
    );
    let expected_owner = PublicKey::parse(&request.owner)
        .context("expected an owner hex public key or npub")?
        .to_hex();
    // Resolving a specific pubkey fails closed when its signing key is absent.
    let (owner, signing_key) = resolve_identity(&expected_owner)?;
    ensure!(owner == expected_owner, "configured signing owner differs");
    let key = signing_key.context("no configured signing key for the expected owner")?;
    let config = Config::load()?;
    let mut client = NostrClient::new(&owner, Some(key), None, false, &config)?;
    ensure!(
        client.pubkey() == expected_owner,
        "unexpected publication owner"
    );
    let candidate_refs = client.prebuilt_git_refs(&request.candidate.hash, &new_key)?;
    ensure!(
        candidate_refs == request.candidate.refs,
        "candidate refs differ from the request"
    );
    // Re-read the signed pointer immediately before publication. The operator holds the writer lock.
    let (refs, hash, observed_key) = client.uncached_signed_git_refs(name)?;
    ensure!(
        hash.as_deref() == Some(request.expected.hash.as_str()),
        "prior signed root hash differs"
    );
    ensure!(
        observed_key == Some(old_key),
        "prior signed root key differs or is absent"
    );
    ensure!(
        refs == request.expected.refs,
        "prior refs differ from the request"
    );
    let (url, relays) = client.publish_repo(
        name,
        &request.candidate.hash,
        Some((new_key.as_bytes(), false, false)),
    )?;
    println!(
        "{}",
        serde_json::json!({"url":url,"hash":request.candidate.hash,"accepted_relays":relays.connected})
    );
    Ok(())
}
