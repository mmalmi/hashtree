use anyhow::{bail, Context, Result};
use hashtree_cli::config::NostrEventTransport;
use hashtree_cli::{Config, NostrKeys, NostrResolverConfig, NostrRootResolver, RootResolver};
use hashtree_core::Cid;
use nostr::Timestamp;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

pub(super) enum ReleasePublisher {
    Relay(NostrRootResolver),
    FipsDaemon {
        client: reqwest::Client,
        base: reqwest::Url,
        keys: NostrKeys,
        relays: Vec<String>,
    },
}

impl ReleasePublisher {
    pub(super) async fn connect(config: &Config, keys: NostrKeys) -> Result<Self> {
        match config.nostr.event_transport {
            NostrEventTransport::Relay => {
                let relays = config.nostr.active_relays();
                if relays.is_empty() {
                    bail!("Release publication requires configured Nostr relays or fips-local-only transport");
                }
                Ok(Self::Relay(
                    NostrRootResolver::new(NostrResolverConfig {
                        relays,
                        resolve_timeout: Duration::from_secs(5),
                        secret_key: Some(keys),
                    })
                    .await?,
                ))
            }
            NostrEventTransport::FipsLocalOnly => {
                let base = daemon_url(&config.server.bind_address)?;
                let client = reqwest::Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(Duration::from_secs(15))
                    .build()?;
                let status: serde_json::Value = client
                    .get(base.join("api/status")?)
                    .send()
                    .await
                    .context("FIPS release publication requires a running local htree daemon")?
                    .error_for_status()?
                    .json()
                    .await?;
                if status["nostr_event_transport"] != "fips-local-only" {
                    bail!("Release publication requires the local daemon to use nostr.event_transport=fips-local-only; restart it with the matching configuration");
                }
                Ok(Self::FipsDaemon {
                    client,
                    base,
                    keys,
                    // Release heads remain visible to already shipped relay
                    // clients, even when ordinary daemon reads use only FIPS.
                    relays: config.nostr.relays.clone(),
                })
            }
        }
    }

    pub(super) async fn resolve(&self, key: &str) -> Result<(Option<Cid>, Option<Timestamp>)> {
        match self {
            Self::Relay(resolver) => Ok((resolver.resolve(key).await?, None)),
            Self::FipsDaemon { client, base, .. } => {
                let (npub, tree_name) = key.split_once('/').context("Invalid release tree key")?;
                let mut url = base.join("api/nostr/resolve/")?;
                url.path_segments_mut()
                    .expect("HTTP URL")
                    .pop_if_empty()
                    .push(npub)
                    .push(tree_name);
                url.set_query(Some("refresh=1"));
                let root: serde_json::Value = client
                    .get(url)
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                let cid = root["cid"].as_str().context(
                    "No existing release root was observed through the FIPS daemon; refusing to overwrite release history. Seed the daemon with the existing signed root event before publishing",
                )?;
                let created_at = root["created_at"]
                    .as_u64()
                    .context("Daemon release root has no signed timestamp")?;
                Ok((
                    Some(Cid::parse(cid)?),
                    Some(Timestamp::from_secs(created_at)),
                ))
            }
        }
    }

    pub(super) async fn publish(
        &self,
        key: &str,
        cid: &Cid,
        latest_created_at: Option<Timestamp>,
    ) -> Result<()> {
        match self {
            Self::Relay(resolver) => {
                let result = resolver.publish(key, cid).await;
                let _ = resolver.stop().await;
                if !result? {
                    bail!("Release publish returned false");
                }
            }
            Self::FipsDaemon {
                client,
                base,
                keys,
                relays,
            } => {
                let (_, tree_name) = key.split_once('/').context("Invalid release tree key")?;
                let event =
                    NostrRootResolver::root_event_builder(tree_name, cid, latest_created_at)
                        .sign_with_keys(keys)?;
                if !NostrRootResolver::event_matches_key(key, &event)? {
                    bail!("Release signing identity does not own this tree");
                }
                client
                    .post(base.join("api/nostr/events?transport=fips-local-only")?)
                    .json(&event)
                    .send()
                    .await?
                    .error_for_status()
                    .context("Local FIPS daemon rejected release publication")?;
                publish_legacy_head(relays, &event).await?;
            }
        }
        Ok(())
    }
}

async fn publish_legacy_head(relays: &[String], event: &nostr::Event) -> Result<()> {
    if relays.is_empty() {
        return Ok(());
    }
    let client = nostr_sdk::Client::default();
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        for relay in relays {
            client.add_relay(relay).await?;
        }
        client.connect().await;
        let output = client.send_event(event).await?;
        if output.success.is_empty() {
            bail!("No configured legacy relay acknowledged the signed release head");
        }
        Ok::<(), anyhow::Error>(())
    })
    .await;
    client.shutdown().await;
    result
        .context("Legacy relay release publication timed out")?
        .context("FIPS accepted the release head, but legacy relay publication failed")
}

fn daemon_url(bind_address: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(&format!("http://{bind_address}/"))?;
    let host = url
        .host_str()
        .context("Daemon address has no host")?
        .trim_matches(['[', ']']);
    if host == "localhost" {
        url.set_ip_host(Ipv4Addr::LOCALHOST.into())
            .map_err(|_| anyhow::anyhow!("Invalid daemon address"))?;
    } else {
        let ip: IpAddr = host
            .parse()
            .context("Release publication requires a loopback daemon address")?;
        if ip.is_unspecified() {
            url.set_ip_host(if ip.is_ipv4() {
                Ipv4Addr::LOCALHOST.into()
            } else {
                Ipv6Addr::LOCALHOST.into()
            })
            .map_err(|_| anyhow::anyhow!("Invalid daemon address"))?;
        } else if !ip.is_loopback() {
            bail!("Release publication requires a loopback daemon address");
        }
    }
    Ok(url)
}

#[cfg(test)]
mod tests;
