use super::{
    state::{State, MAX_AUTHORS},
    Query, SearchResults,
};
use crate::nostr_client::{load_keys, resolve_self_identity};
use anyhow::{ensure, Context, Result};
use hashtree_config::Config;
use nostr::{Filter, Kind, PublicKey};
use nostr_pubsub_relay::RelayEventBus;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

pub struct SearchOptions {
    pub relays: Vec<String>,
    pub roots: Vec<PublicKey>,
    pub window: Duration,
    pub local_follows: Option<Vec<PublicKey>>,
    pub local_mutes: Vec<PublicKey>,
}

pub async fn search(
    input: &str,
    git: bool,
    window: Duration,
    data_dir: Option<&Path>,
) -> Result<SearchResults> {
    let config = Config::load()?;
    let own_key = resolve_self_identity(&load_keys())
        .map(|(pk, _)| PublicKey::from_hex(&pk))
        .transpose()?;
    let mut roots = if let Some(root) = &config.nostr.socialgraph_root {
        vec![PublicKey::parse(root).context("Invalid social graph root")?]
    } else if let Some(root) = own_key {
        vec![root]
    } else {
        config
            .nostr
            .bootstrap_follows
            .iter()
            .map(|pk| PublicKey::parse(pk))
            .collect::<Result<Vec<_>, _>>()?
    };
    let directory = data_dir
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HTREE_DATA_DIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(&config.storage.data_dir));
    let mut local_follows = if roots.len() == 1 && Some(roots[0]) == own_key {
        super::local::list(&directory.join("contacts.json"))?
    } else {
        None
    };
    let local_mutes = super::local::list(&directory.join("mutes.json"))?.unwrap_or_default();
    let mut query = Query::parse(input)?;
    // A named local alias is already a publisher choice, including in listings.
    if query.owner.is_some() {
        if let Some(target) = super::explicit_target(input)? {
            query = Query::parse(&target)?;
            roots = vec![PublicKey::parse(
                query.owner.as_deref().context("Publisher required")?,
            )?];
            local_follows = None;
        }
    }
    let local_only = std::env::var("HTREE_LOCAL_DAEMON_ONLY").is_ok_and(|s| {
        !matches!(
            s.to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        )
    });
    let relays = if local_only {
        hashtree_config::detect_local_relay_urls(Some(&config.server.bind_address))
    } else {
        hashtree_config::resolve_relays(&config.nostr.relays, Some(&config.server.bind_address))
    };
    search_with(
        &query,
        git,
        SearchOptions {
            relays,
            roots,
            window,
            local_follows,
            local_mutes,
        },
    )
    .await
}

/// Observe one bounded window, retaining subscriptions after EOSE and quiet periods.
/// Each update is evaluated against the latest follow/mute lists, not accumulated trust.
pub async fn search_with(
    query: &Query,
    git: bool,
    options: SearchOptions,
) -> Result<SearchResults> {
    ensure!(
        !options.roots.is_empty(),
        "No social graph starting point; set your identity or configure nostr.socialgraph_root"
    );
    ensure!(
        !options.relays.is_empty(),
        "No discovery sources available; start htree or configure Nostr relays"
    );
    ensure!(
        options.window > Duration::ZERO && options.window <= Duration::from_secs(60),
        "Search window must be between 0 and 60 seconds"
    );
    let _ = rustls::crypto::ring::default_provider().install_default();
    let bus = RelayEventBus::new(options.relays, options.window).await?;
    let mut state = State::new(options.roots);
    state.local_follows = options.local_follows.map(|keys| keys.into_iter().collect());
    state.local_mutes = options.local_mutes.into_iter().collect();
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1024);
    let overflow = Arc::new(AtomicBool::new(false));
    let mut subscribed = BTreeSet::new();
    let mut subscriptions = Vec::new();
    let deadline = tokio::time::sleep(options.window);
    tokio::pin!(deadline);
    let mut planner = tokio::time::interval(Duration::from_millis(50));
    planner.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let result: Result<()> = async {
        loop {
            tokio::select! {
                biased;
                () = &mut deadline => break,
                _ = planner.tick() => {
                    let mut authors: Vec<_> = state.distances().into_iter().collect();
                    authors.sort_by_key(|(pk, distance)| (*distance, *pk));
                    state.truncated |= authors.len() > MAX_AUTHORS;
                    let remaining = MAX_AUTHORS.saturating_sub(subscribed.len());
                    let new: Vec<_> = authors.into_iter().map(|(pk, _)| pk).filter(|pk| !subscribed.contains(pk)).take(remaining).collect();
                    for chunk in new.chunks(64) {
                        let filters = vec![
                            Filter::new().authors(chunk.iter().copied()).kinds([Kind::Metadata, Kind::ContactList, Kind::MuteList]).limit(192),
                            Filter::new().authors(chunk.iter().copied()).kinds(crate::nostr_client::hashtree_root_kinds()).limit(512),
                        ];
                        let sender = sender.clone();
                        let full = overflow.clone();
                        let subscription = bus.subscribe_with_admission(filters, Arc::new(move |event| {
                            if sender.try_send(event.event.into_event()).is_err() { full.store(true, Ordering::Relaxed); return false; }
                            true
                        })).await?;
                        subscriptions.push(subscription);
                        subscribed.extend(chunk.iter().copied());
                    }
                },
                Some(event) = receiver.recv() => state.ingest(event),
            }
        }
        Ok(())
    }.await;
    for subscription in subscriptions {
        let _ = subscription.close().await;
    }
    bus.client().disconnect().await;
    result?;
    state.truncated |= overflow.load(Ordering::Relaxed);
    Ok(state.results(query, git))
}
