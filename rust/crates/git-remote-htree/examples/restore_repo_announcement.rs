use anyhow::{ensure, Context, Result};
use futures::future::try_join_all;
use git_remote_htree::nostr_client::{resolve_identity, KIND_REPO_ANNOUNCEMENT};
use hashtree_config::{resolve_relays, Config};
use nostr_relay_pool::{RelayNotification, SubscribeOptions};
use nostr_sdk::prelude::*;
use serde::Deserialize;
use std::{cmp::Reverse, time::Duration};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    owner: String,
    repository: String,
    original: Event,
    current: Event,
}

fn validate_announcement(event: &Event, owner: PublicKey, name: &str) -> Result<()> {
    event
        .verify()
        .context("invalid announcement signature or ID")?;
    ensure!(
        event.kind.as_u16() == KIND_REPO_ANNOUNCEMENT,
        "unexpected announcement kind"
    );
    ensure!(event.pubkey == owner, "announcement owner differs");
    let mut identifiers = event.tags.iter().filter(|t| t.as_slice()[0] == "d");
    let identifier = identifiers.next().context("missing repository name")?;
    ensure!(
        identifier.as_slice().len() == 2
            && identifier.content() == Some(name)
            && identifiers.next().is_none(),
        "repository name differs or is ambiguous"
    );
    Ok(())
}

fn validate_repair(request: &Request, owner: PublicKey) -> Result<()> {
    let original = &request.original;
    let current = &request.current;
    let name = &request.repository;
    ensure!(
        !name.chars().any(char::is_control)
            && name
                .split('/')
                .all(|s| !s.is_empty() && s != "." && s != ".."),
        "invalid repository name"
    );
    validate_announcement(original, owner, name)?;
    validate_announcement(current, owner, name)?;
    ensure!(
        original.created_at < current.created_at,
        "original must precede current"
    );
    ensure!(
        original.content == current.content,
        "announcement content changed"
    );
    ensure!(
        original.tags.len() == current.tags.len(),
        "announcement tag count changed"
    );
    let mut changes = 0;
    for (before, after) in original.tags.iter().zip(current.tags.iter()) {
        let old = before.as_slice();
        let new = after.as_slice();
        if old[0] == "relays" && new[0] == "relays" {
            ensure!(old.len() >= 2 && new.len() >= 2, "empty relay list");
            for value in old.iter().skip(1).chain(new.iter().skip(1)) {
                RelayUrl::parse(value).context("invalid advertised relay")?;
            }
            ensure!(old != new, "relay lists already match");
            changes += 1;
        } else {
            ensure!(old == new, "a tag other than the relay list changed");
        }
    }
    ensure!(changes == 1, "expected exactly one changed relay list");
    Ok(())
}

async fn restore(request: Request) -> Result<()> {
    let owner = PublicKey::parse(&request.owner).context("invalid expected owner")?;
    validate_repair(&request, owner)?;
    let (_, signing_key) = resolve_identity(&owner.to_hex())?;
    let secret = signing_key.context("no configured signing key for the expected owner")?;
    let keys = Keys::new(SecretKey::parse(&secret)?);
    ensure!(
        keys.public_key() == owner,
        "configured signing owner differs"
    );

    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = Config::load()?;
    let relays = resolve_relays(&config.nostr.relays, Some(&config.server.bind_address));
    ensure!(!relays.is_empty(), "no configured transport relays");
    let client = Client::default();
    for url in &relays {
        client.add_relay(url).await?;
    }
    let wait = Duration::from_secs(8);
    let filter = Filter::new()
        .kind(Kind::Custom(KIND_REPO_ANNOUNCEMENT))
        .author(owner)
        .identifier(&request.repository);
    let current = &request.current;
    // Every configured transport must return the reviewed predecessor as its latest event.
    // The caller holds the repository writer lock across this operation.
    try_join_all(relays.iter().map(|url| {
        let client = &client;
        let filter = filter.clone();
        let name = &request.repository;
        async move {
            client.try_connect_relay(url, wait).await?;
            let relay = client.relay(url).await?;
            let mut notifications = relay.notifications();
            let id = relay.subscribe(filter, SubscribeOptions::default()).await?;
            let winner = tokio::time::timeout(wait, async {
                let mut latest = None;
                loop {
                    let RelayNotification::Message { message } = notifications.recv().await? else {
                        continue;
                    };
                    match message {
                        RelayMessage::Event {
                            subscription_id,
                            event,
                        } if subscription_id.as_ref() == &id => {
                            validate_announcement(event.as_ref(), owner, name)?;
                            latest = latest.max(Some((event.created_at, Reverse(event.id))));
                        }
                        RelayMessage::EndOfStoredEvents(subscription_id)
                            if subscription_id.as_ref() == &id =>
                        {
                            return Ok::<_, anyhow::Error>(latest);
                        }
                        RelayMessage::Closed {
                            subscription_id, ..
                        } if subscription_id.as_ref() == &id => {
                            anyhow::bail!("announcement query closed before EOSE on {url}");
                        }
                        _ => {}
                    }
                }
            })
            .await
            .context("relay did not finish announcement query")??;
            relay.unsubscribe(&id).await?;
            ensure!(
                winner.is_some_and(|(_, id)| id.0 == current.id),
                "live predecessor differs or is absent on {url}"
            );
            Ok::<(), anyhow::Error>(())
        }
    }))
    .await?;

    let next = current
        .created_at
        .as_secs()
        .checked_add(1)
        .context("timestamp exhausted")?;
    let original = &request.original;
    let created_at = Timestamp::now().max(Timestamp::from_secs(next));
    let event = UnsignedEvent::new(
        owner,
        created_at,
        Kind::Custom(KIND_REPO_ANNOUNCEMENT),
        original.tags.clone(),
        &original.content,
    )
    .sign_with_keys(&keys)?;
    validate_announcement(&event, owner, &request.repository)?;
    ensure!(
        event.tags.as_slice() == original.tags.as_slice() && event.content == original.content,
        "signed payload differs from original"
    );
    let output = client.send_event_to(relays, &event).await?;
    ensure!(
        !output.success.is_empty(),
        "no relay accepted the restored announcement"
    );
    println!(
        "{}",
        serde_json::json!({"event":event,"previous":current.id,"accepted_relays":output.success,"failed_relays":output.failed})
    );
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let request: Request =
        serde_json::from_reader(std::io::stdin().lock()).context("invalid JSON on stdin")?;
    tokio::time::timeout(Duration::from_secs(40), restore(request))
        .await
        .context("announcement restoration timed out")?
}
