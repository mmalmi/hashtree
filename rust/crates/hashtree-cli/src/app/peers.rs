use anyhow::Result;
use hashtree_cli::Config;

use super::util::format_bytes;

/// List connected peers with optional profile resolution.
pub(crate) async fn list_peers(addr: &str) -> Result<()> {
    use nostr::nips::nip19::ToBech32;
    use nostr::PublicKey;

    let url = format!("http://{}/api/peers", addr);
    let resp = match reqwest::get(&url).await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            eprintln!("Daemon returned error: {}", r.status());
            return Ok(());
        }
        Err(_) => {
            eprintln!("Daemon not running at {}", addr);
            eprintln!("Start with: htree start");
            return Ok(());
        }
    };

    let data: serde_json::Value = resp.json().await?;

    if !data
        .get("enabled")
        .and_then(|e| e.as_bool())
        .unwrap_or(false)
    {
        println!("Peer router is not enabled");
        return Ok(());
    }

    let peers = data.get("peers").and_then(|p| p.as_array());
    let Some(peers) = peers else {
        println!("No peers");
        return Ok(());
    };

    let total = data
        .get("total")
        .and_then(|value| value.as_u64())
        .unwrap_or(peers.len() as u64);
    let connected_count = data
        .get("connected")
        .and_then(|value| value.as_u64())
        .unwrap_or_else(|| {
            peers
                .iter()
                .filter(|p| {
                    p.get("state")
                        .and_then(|s| s.as_str())
                        .map(|s| s.eq_ignore_ascii_case("connected"))
                        .unwrap_or(false)
                })
                .count() as u64
        });
    println!("Peer router: {connected_count}/{total} connected");

    let bytes_sent = data.get("bytes_sent").and_then(|b| b.as_u64()).unwrap_or(0);
    let bytes_received = data
        .get("bytes_received")
        .and_then(|b| b.as_u64())
        .unwrap_or(0);
    if bytes_sent > 0 || bytes_received > 0 {
        println!(
            "Traffic: up {}, down {}",
            format_bytes(bytes_sent),
            format_bytes(bytes_received)
        );
    }

    let mesh_received = data
        .get("mesh_received")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let mesh_forwarded = data
        .get("mesh_forwarded")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let mesh_dropped_duplicate = data
        .get("mesh_dropped_duplicate")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    if mesh_received > 0 || mesh_forwarded > 0 || mesh_dropped_duplicate > 0 {
        println!(
            "Mesh frames: received {}, forwarded {}, duplicate drops {}",
            mesh_received, mesh_forwarded, mesh_dropped_duplicate
        );
    }

    // Collect connected peers
    let connected: Vec<_> = peers
        .iter()
        .filter(|p| {
            p.get("state")
                .and_then(|s| s.as_str())
                .map(|s| s.eq_ignore_ascii_case("connected"))
                .unwrap_or(false)
        })
        .collect();

    if connected.is_empty() {
        println!("No connected peers");
        return Ok(());
    }

    println!("\nConnected peers:");

    // Load config for relays
    let config = Config::load()?;

    // Group peers by pool
    let follows: Vec<_> = connected
        .iter()
        .filter(|p| {
            p.get("pool")
                .and_then(|s| s.as_str())
                .map(|s| s.eq_ignore_ascii_case("follows"))
                .unwrap_or(false)
        })
        .collect();
    let others: Vec<_> = connected
        .iter()
        .filter(|p| {
            !p.get("pool")
                .and_then(|s| s.as_str())
                .map(|s| s.eq_ignore_ascii_case("follows"))
                .unwrap_or(false)
        })
        .collect();

    // Helper to print peer with profile
    async fn print_peer(peer: &serde_json::Value, relays: &[String]) {
        let pubkey_hex = peer.get("pubkey").and_then(|p| p.as_str()).unwrap_or("");

        let npub = if let Ok(pk) = PublicKey::from_hex(pubkey_hex) {
            pk.to_bech32().unwrap_or_else(|_| pubkey_hex.to_string())
        } else {
            pubkey_hex.to_string()
        };

        let profile_name = fetch_profile_name(relays, pubkey_hex).await;
        let transport = peer
            .get("transport")
            .and_then(|v| v.as_str())
            .unwrap_or("webrtc");
        let signal_paths = peer
            .get("signal_paths")
            .and_then(|v| v.as_array())
            .map(|paths| {
                paths
                    .iter()
                    .filter_map(|path| path.as_str())
                    .collect::<Vec<_>>()
                    .join("+")
            })
            .filter(|paths| !paths.is_empty());

        // Get bandwidth stats
        let bytes_sent = peer.get("bytes_sent").and_then(|b| b.as_u64()).unwrap_or(0);
        let bytes_received = peer
            .get("bytes_received")
            .and_then(|b| b.as_u64())
            .unwrap_or(0);

        let name_part = if let Some(name) = profile_name {
            format!(" ({})", name)
        } else {
            String::new()
        };

        let transport_part = match signal_paths {
            Some(paths) => format!(" [{} via {}]", transport, paths),
            None => format!(" [{}]", transport),
        };

        let bandwidth_part = if bytes_sent > 0 || bytes_received > 0 {
            format!(
                " [\u{2191}{} \u{2193}{}]",
                format_bytes(bytes_sent),
                format_bytes(bytes_received)
            )
        } else {
            String::new()
        };

        println!(
            "  {}{}{}{}",
            npub, name_part, transport_part, bandwidth_part
        );
    }

    if !follows.is_empty() {
        println!("Follows:");
        for peer in follows {
            print_peer(peer, &config.nostr.relays).await;
        }
        if !others.is_empty() {
            println!();
        }
    }

    if !others.is_empty() {
        println!("Other:");
        for peer in others {
            print_peer(peer, &config.nostr.relays).await;
        }
    }

    Ok(())
}

/// Fetch profile name from Nostr relays (2s timeout).
pub(crate) async fn fetch_profile_name(relays: &[String], pubkey_hex: &str) -> Option<String> {
    use nostr::{Filter, Kind, PublicKey};
    use nostr_sdk::ClientBuilder;
    use std::time::Duration;

    let pk = PublicKey::from_hex(pubkey_hex).ok()?;

    // Create client with relays
    let client = ClientBuilder::default().build();
    for relay in relays {
        let _ = client.add_relay(relay).await;
    }
    client.connect().await;

    // Fetch kind 0 profile
    let filter = Filter::new().author(pk).kind(Kind::Metadata).limit(1);

    let timeout = Duration::from_secs(2);
    let events = tokio::time::timeout(timeout, client.fetch_events(filter, timeout))
        .await
        .ok()?
        .ok()?
        .to_vec();
    let _ = client.disconnect().await;

    // Parse profile JSON
    let event = events.into_iter().next()?;
    let profile: serde_json::Value = serde_json::from_str(&event.content).ok()?;

    // Try display_name, then name, then username
    profile
        .get("display_name")
        .or_else(|| profile.get("name"))
        .or_else(|| profile.get("username"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}
