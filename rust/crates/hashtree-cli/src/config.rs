use anyhow::{Context, Result};
use nostr::nips::nip19::{FromBech32, ToBech32};
use nostr::{Keys, SecretKey};
use serde::{Deserialize, Serialize};
use std::fs;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub nostr: NostrConfig,
    #[serde(default)]
    pub blossom: BlossomConfig,
    #[serde(default)]
    pub sync: SyncConfig,
    #[serde(default)]
    pub cashu: CashuConfig,
    #[serde(default)]
    pub updater: UpdaterConfig,
}

/// htree self-update preferences. Auto-check is on by default — htree
/// quietly checks for a newer published binary at most once per
/// `check_interval_hours` and prints a one-liner to stderr when one
/// exists. `auto_install` is off by default; flip it on to install in
/// the background and print the result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdaterConfig {
    #[serde(default = "default_auto_check")]
    pub auto_check: bool,
    #[serde(default)]
    pub auto_install: bool,
    #[serde(default = "default_check_interval_hours")]
    pub check_interval_hours: u32,
}

impl Default for UpdaterConfig {
    fn default() -> Self {
        Self {
            auto_check: default_auto_check(),
            auto_install: false,
            check_interval_hours: default_check_interval_hours(),
        }
    }
}

fn default_auto_check() -> bool {
    true
}

fn default_check_interval_hours() -> u32 {
    24
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServerMode {
    #[default]
    Normal,
    #[serde(alias = "signal-only")]
    Assist,
}

impl ServerMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Assist => "assist",
        }
    }

    pub const fn hash_get_enabled(self) -> bool {
        matches!(self, Self::Normal)
    }

    pub const fn background_services_enabled(self) -> bool {
        matches!(self, Self::Normal)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default)]
    pub mode: ServerMode,
    #[serde(default = "default_bind_address")]
    pub bind_address: String,
    #[serde(default = "default_enable_auth")]
    pub enable_auth: bool,
    /// Port for the built-in STUN server (0 = disabled)
    #[serde(default = "default_stun_port")]
    pub stun_port: u16,
    /// Enable WebRTC P2P connections
    #[serde(default = "default_enable_webrtc")]
    pub enable_webrtc: bool,
    /// Enable FIPS-backed Hashtree blob exchange.
    #[serde(default = "default_enable_fips")]
    pub enable_fips: bool,
    /// FIPS discovery/signaling scope for Hashtree peers.
    #[serde(default = "default_fips_discovery_scope")]
    pub fips_discovery_scope: String,
    /// Maximum unconfigured peers allowed to enter Nostr discovery concurrently.
    /// Zero keeps discovery restricted to configured/social-graph peers.
    #[serde(default)]
    pub fips_open_discovery_max_pending: usize,
    /// Optional loopback rendezvous address override for isolated local stacks.
    /// Unset uses the FIPS well-known address (127.0.0.1:21211).
    #[serde(default)]
    pub fips_local_rendezvous_addr: Option<String>,
    /// Enable FIPS LAN/mDNS peer discovery.
    #[serde(default = "default_enable_fips_lan_discovery")]
    pub enable_fips_lan_discovery: bool,
    /// FIPS Nostr relays used for discovery adverts and encrypted signaling.
    /// Unset uses active [nostr].relays plus FIPS defaults; an explicit empty
    /// list disables relay discovery.
    #[serde(default)]
    pub fips_relays: Option<Vec<String>>,
    /// Always-configured Hashtree FIPS peers. These are useful for origin/cache
    /// pairs that should connect immediately without waiting for discovery.
    #[serde(
        default,
        alias = "preconfigured_fips_peers",
        alias = "fips_static_peers"
    )]
    pub fips_peers: Vec<ConfiguredFipsPeer>,
    /// Enable ordinary FIPS UDP endpoint transport.
    #[serde(default = "default_enable_fips_udp")]
    pub enable_fips_udp: bool,
    /// FIPS UDP bind address. Empty/default lets the kernel pick an ephemeral port.
    #[serde(default)]
    pub fips_udp_bind_addr: Option<String>,
    /// Advertise the FIPS UDP endpoint as directly reachable.
    #[serde(default)]
    pub fips_udp_public: bool,
    /// Explicit FIPS UDP address to advertise when `fips_udp_public` is true.
    #[serde(default)]
    pub fips_udp_external_addr: Option<String>,
    /// Enable FIPS WebRTC endpoint transport.
    #[serde(default = "default_enable_fips_webrtc")]
    pub enable_fips_webrtc: bool,
    /// Host-local Ethernet interfaces for FIPS endpoint transport.
    #[serde(default)]
    pub fips_ethernet_interfaces: Vec<String>,
    /// Allow daemon cache misses to fetch blobs from FIPS peers.
    #[serde(default = "default_fetch_from_fips_peers", alias = "http_fips_fetch")]
    pub fetch_from_fips_peers: bool,
    /// How long one FIPS blob request waits for a valid response.
    #[serde(default = "default_fips_request_timeout_ms")]
    pub fips_request_timeout_ms: u64,
    /// Allow HTTP misses to fetch blobs from connected WebRTC peers.
    #[serde(default = "default_http_webrtc_fetch")]
    pub http_webrtc_fetch: bool,
    /// Explicit daemon endpoint URLs this node may share privately with connected peers
    /// for WebRTC signaling handoff.
    #[serde(default, alias = "peer_direct_urls", alias = "peer_advertise_urls")]
    pub peer_signal_urls: Vec<String>,
    /// Enable LAN multicast discovery/signaling for native peers.
    #[serde(default = "default_enable_multicast")]
    pub enable_multicast: bool,
    /// IPv4 multicast group used for LAN discovery/signaling.
    #[serde(default = "default_multicast_group")]
    pub multicast_group: String,
    /// UDP port used for LAN multicast discovery/signaling.
    #[serde(default = "default_multicast_port")]
    pub multicast_port: u16,
    /// Maximum peers admitted from LAN multicast discovery.
    /// Set to 0 to disable multicast even when enable_multicast is true.
    #[serde(default = "default_max_multicast_peers")]
    pub max_multicast_peers: usize,
    /// Enable Android Wi-Fi Aware nearby discovery/signaling for native peers.
    #[serde(default = "default_enable_wifi_aware")]
    pub enable_wifi_aware: bool,
    /// Maximum peers admitted from Wi-Fi Aware discovery.
    /// Set to 0 to disable Wi-Fi Aware even when enable_wifi_aware is true.
    #[serde(default = "default_max_wifi_aware_peers")]
    pub max_wifi_aware_peers: usize,
    /// Enable native Bluetooth discovery/transport for nearby peers.
    #[serde(default = "default_enable_bluetooth")]
    pub enable_bluetooth: bool,
    /// Maximum peers admitted from Bluetooth discovery.
    /// Set to 0 to disable Bluetooth even when enable_bluetooth is true.
    #[serde(default = "default_max_bluetooth_peers")]
    pub max_bluetooth_peers: usize,
    /// Allow anyone with valid Nostr auth to write (default: true)
    /// When false, only social graph members can write
    #[serde(default = "default_public_writes")]
    pub public_writes: bool,
    /// Allow public plaintext reads from mutable npub routes (default: true)
    /// When false, only configured or social graph approved npubs are served.
    #[serde(default = "default_public_plaintext_reads")]
    pub public_plaintext_reads: bool,
    /// Allow public access to social graph snapshot endpoint (default: false)
    #[serde(default = "default_socialgraph_snapshot_public")]
    pub socialgraph_snapshot_public: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfiguredFipsPeer {
    pub npub: String,
    #[serde(default)]
    pub udp_addresses: Vec<String>,
}

fn default_public_writes() -> bool {
    true
}

fn default_public_plaintext_reads() -> bool {
    false
}

fn default_socialgraph_snapshot_public() -> bool {
    false
}

impl ServerConfig {
    pub fn resolved_fips_relays(&self, active_nostr_relays: &[String]) -> Vec<String> {
        match &self.fips_relays {
            // An explicit list is authoritative. This is required for private
            // relay deployments and deterministic local test networks; adding
            // public bootstrap relays here leaks signaling outside that scope.
            Some(relays) => normalize_fips_signal_relays(relays),
            None => merge_fips_signal_relays(active_nostr_relays),
        }
    }
}

const DEFAULT_FIPS_SIGNAL_RELAYS: [&str; 2] = ["wss://temp.iris.to", "wss://relay.primal.net"];

fn merge_fips_signal_relays(configured: &[String]) -> Vec<String> {
    normalize_fips_signal_relays(
        &configured
            .iter()
            .cloned()
            .chain(DEFAULT_FIPS_SIGNAL_RELAYS.into_iter().map(str::to_string))
            .collect::<Vec<_>>(),
    )
}

fn normalize_fips_signal_relays(configured: &[String]) -> Vec<String> {
    let mut relays = Vec::new();
    for relay in configured {
        let normalized = relay.trim().trim_end_matches('/').to_string();
        if normalized.is_empty() || relays.contains(&normalized) {
            continue;
        }
        relays.push(normalized);
    }
    relays
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    #[serde(default = "default_max_size_gb")]
    pub max_size_gb: u64,
    #[serde(default = "default_storage_evict_orphans")]
    pub evict_orphans: bool,
    /// Optional S3/R2 backend for blob storage
    #[serde(default)]
    pub s3: Option<S3Config>,
}

/// S3-compatible storage configuration (works with AWS S3, Cloudflare R2, MinIO, etc.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3Config {
    /// S3 endpoint URL (e.g., "https://<account_id>.r2.cloudflarestorage.com" for R2)
    pub endpoint: String,
    /// S3 bucket name
    pub bucket: String,
    /// Optional key prefix for all blobs (e.g., "blobs/")
    #[serde(default)]
    pub prefix: Option<String>,
    /// AWS region (use "auto" for R2)
    #[serde(default = "default_s3_region")]
    pub region: String,
    /// Access key ID (can also be set via AWS_ACCESS_KEY_ID env var)
    #[serde(default)]
    pub access_key: Option<String>,
    /// Secret access key (can also be set via AWS_SECRET_ACCESS_KEY env var)
    #[serde(default)]
    pub secret_key: Option<String>,
    /// Public URL for serving blobs (optional, for generating public URLs)
    #[serde(default)]
    pub public_url: Option<String>,
}

fn default_s3_region() -> String {
    "auto".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum NostrEventTransport {
    #[default]
    Relay,
    FipsLocalOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NostrConfig {
    #[serde(default = "default_nostr_enabled")]
    pub enabled: bool,
    #[serde(default = "default_relays")]
    pub relays: Vec<String>,
    /// Provider used for Hashtree root/site event lookup and publication.
    #[serde(default)]
    pub event_transport: NostrEventTransport,
    /// List of npubs allowed to write (blossom uploads). If empty, uses public_writes setting.
    #[serde(default)]
    pub allowed_npubs: Vec<String>,
    /// Social graph root pubkey (npub). Defaults to own key if not set.
    #[serde(default)]
    pub socialgraph_root: Option<String>,
    /// Pubkeys to seed into contacts.json when a new identity is initialized.
    /// Set to [] to opt out.
    #[serde(default = "default_nostr_bootstrap_follows")]
    pub bootstrap_follows: Vec<String>,
    /// How many hops to crawl the social graph (default: 2)
    #[serde(default = "default_social_graph_crawl_depth", alias = "crawl_depth")]
    pub social_graph_crawl_depth: u32,
    /// Max follow distance to mirror into public event/profile indexes.
    /// Defaults to social_graph_crawl_depth when unset.
    #[serde(default)]
    pub mirror_max_follow_distance: Option<u32>,
    /// Max follow distance for write access (default: 3)
    #[serde(default = "default_max_write_distance")]
    pub max_write_distance: u32,
    /// Max size for the trusted social graph store in GB (default: 10)
    #[serde(default = "default_nostr_db_max_size_gb")]
    pub db_max_size_gb: u64,
    /// Max size for the social graph spambox in GB (default: 1)
    /// Set to 0 for memory-only spambox (no on-disk DB)
    #[serde(default = "default_nostr_spambox_max_size_gb")]
    pub spambox_max_size_gb: u64,
    /// Require relays to support NIP-77 negentropy for mirror history sync.
    #[serde(default)]
    pub negentropy_only: bool,
    /// Threshold for treating a user as overmuted in mirrored profile indexing/search.
    #[serde(default = "default_nostr_overmute_threshold")]
    pub overmute_threshold: f64,
    /// Kinds mirrored from upstream relays for the trusted hashtree index.
    #[serde(default = "default_nostr_mirror_kinds")]
    pub mirror_kinds: Vec<u16>,
    /// Authors per ordinary history chunk and between durable mirror-root publications.
    /// Archive sync caps each durable chunk to one configured relay author batch.
    #[serde(default = "default_nostr_history_sync_author_chunk_size")]
    pub history_sync_author_chunk_size: usize,
    /// Maximum mirrored history events to fetch per author during history sync.
    #[serde(default = "default_nostr_history_sync_per_author_event_limit")]
    pub history_sync_per_author_event_limit: usize,
    /// Run a catch-up history sync after relay reconnects.
    #[serde(default = "default_nostr_history_sync_on_reconnect")]
    pub history_sync_on_reconnect: bool,
    /// Legacy maximum follow distance for complete kind-1 and kind-30023 text history.
    /// Set to null to disable the legacy text-only archive pass.
    #[serde(default = "default_nostr_full_text_note_history_follow_distance")]
    pub full_text_note_history_follow_distance: Option<u32>,
    /// Legacy maximum relay pages per author and text kind for startup history fetches.
    /// Set to 0 to disable the legacy text-only archive pass.
    #[serde(default = "default_nostr_full_text_note_history_max_relay_pages")]
    pub full_text_note_history_max_relay_pages: usize,
    /// Maximum follow distance for the complete post, deletion, repost, reaction, zap,
    /// comment, picture, and article archive.
    /// Used instead of the legacy text-only settings when archive_history_max_relay_pages
    /// is greater than zero. Set to null to disable the complete archive pass.
    #[serde(default = "default_nostr_archive_history_follow_distance")]
    pub archive_history_follow_distance: Option<u32>,
    /// Maximum relay pages per author and kind for the complete startup archive pass.
    /// Set to 0 to leave the new archive pass disabled; bounded recent sync still runs.
    #[serde(default = "default_nostr_archive_history_max_relay_pages")]
    pub archive_history_max_relay_pages: usize,
    /// Enable experimental decentralized Nostr event pubsub when compiled with
    /// the experimental-decentralized-pubsub feature.
    #[serde(default, alias = "relayless_pubsub")]
    pub decentralized_pubsub: bool,
    /// Maximum encoded Nostr event frame accepted on decentralized pubsub.
    /// Values above the authenticated FIPS datagram limit are clamped.
    #[serde(default = "default_nostr_decentralized_pubsub_max_event_bytes")]
    pub decentralized_pubsub_max_event_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlossomConfig {
    #[serde(default = "default_blossom_enabled")]
    pub enabled: bool,
    /// File servers for push/pull (legacy, both read and write)
    #[serde(default)]
    pub servers: Vec<String>,
    /// Read-only file servers (fallback for fetching content)
    #[serde(default = "default_read_servers")]
    pub read_servers: Vec<String>,
    /// Write-enabled file servers (for uploading)
    #[serde(default = "default_write_servers")]
    pub write_servers: Vec<String>,
    /// Maximum upload size in MB (default: 5)
    #[serde(default = "default_max_upload_mb")]
    pub max_upload_mb: u64,
    /// Require public Blossom and peer-fetched cached blobs to look encrypted.
    #[serde(default = "default_require_random_untrusted_ingest")]
    pub require_random_untrusted_ingest: bool,
    /// Return from Blossom PUT /upload after validation and queue local storage
    /// writes in the background.
    #[serde(default = "default_optimistic_uploads")]
    pub optimistic_uploads: bool,
    /// Background write-behind targets for blobs accepted by this server.
    /// Useful for hot-cache origins that should ACK local writes quickly and
    /// replicate them to a larger origin without blocking the client.
    #[serde(
        default,
        alias = "write_behind_servers",
        alias = "mirror_write_servers"
    )]
    pub replicate_servers: Vec<String>,
    /// Maximum in-memory upload body bytes waiting for background replication.
    #[serde(default = "default_replicate_queue_mb")]
    pub replicate_queue_mb: u64,
}

impl BlossomConfig {
    pub fn all_read_servers(&self) -> Vec<String> {
        if !self.enabled {
            return Vec::new();
        }
        let mut servers = self.servers.clone();
        servers.extend(self.read_servers.clone());
        servers.extend(self.write_servers.clone());
        if servers.is_empty() {
            servers = default_read_servers();
            servers.extend(default_write_servers());
        }
        servers.sort();
        servers.dedup();
        servers
    }

    /// Read alternatives for the daemon's HTTP server, excluding the server
    /// itself. CLI clients may intentionally use the local daemon as their
    /// configured Blossom endpoint, but the daemon must not recursively query
    /// that endpoint when a blob is absent locally.
    pub fn upstream_read_servers(&self, bind_address: &str) -> Vec<String> {
        let Some((bind_host, bind_port)) = parse_bind_authority(bind_address) else {
            return self.all_read_servers();
        };

        self.all_read_servers()
            .into_iter()
            .filter(|server| !is_bound_http_server(server, &bind_host, bind_port))
            .collect()
    }

    pub fn all_write_servers(&self) -> Vec<String> {
        if !self.enabled {
            return Vec::new();
        }
        let mut servers = self.servers.clone();
        servers.extend(self.write_servers.clone());
        if servers.is_empty() {
            servers = default_write_servers();
        }
        servers.sort();
        servers.dedup();
        servers
    }
}

fn parse_bind_authority(bind_address: &str) -> Option<(String, u16)> {
    if let Ok(address) = bind_address.parse::<SocketAddr>() {
        return Some((address.ip().to_string(), address.port()));
    }

    let url = reqwest::Url::parse(&format!("http://{bind_address}")).ok()?;
    Some((url.host_str()?.to_string(), url.port_or_known_default()?))
}

fn is_bound_http_server(server: &str, bind_host: &str, bind_port: u16) -> bool {
    let Ok(url) = reqwest::Url::parse(server) else {
        return false;
    };
    if url.scheme() != "http" || url.port_or_known_default() != Some(bind_port) {
        return false;
    }
    let Some(upstream_host) = url.host_str() else {
        return false;
    };
    let upstream_host = upstream_host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(upstream_host);
    if upstream_host.eq_ignore_ascii_case(bind_host) {
        return true;
    }

    let bind_ip = bind_host.parse::<IpAddr>().ok();
    let upstream_ip = upstream_host.parse::<IpAddr>().ok();
    let upstream_is_localhost = upstream_host.eq_ignore_ascii_case("localhost");
    match bind_ip {
        Some(ip) if ip.is_unspecified() => {
            upstream_is_localhost
                || upstream_ip
                    .is_some_and(|candidate| candidate.is_loopback() || candidate.is_unspecified())
        }
        Some(ip) if ip.is_loopback() => {
            upstream_is_localhost || upstream_ip.is_some_and(|candidate| candidate.is_loopback())
        }
        _ => false,
    }
}

impl NostrConfig {
    pub fn active_relays(&self) -> Vec<String> {
        if self.enabled && self.event_transport == NostrEventTransport::Relay {
            self.relays.clone()
        } else {
            Vec::new()
        }
    }

    pub fn decentralized_pubsub_enabled(&self) -> bool {
        self.enabled
            && self.decentralized_pubsub
            && cfg!(feature = "experimental-decentralized-pubsub")
    }
}

fn default_nostr_decentralized_pubsub_max_event_bytes() -> usize {
    nostr_pubsub_fips::FIPS_NOSTR_PUBSUB_MAX_DATAGRAM_BYTES
}

// Keep in sync with hashtree-config/src/lib.rs
fn default_read_servers() -> Vec<String> {
    let mut servers = vec![
        "https://blossom.primal.net".to_string(),
        "https://cdn.iris.to".to_string(),
    ];
    servers.sort();
    servers
}

fn default_write_servers() -> Vec<String> {
    vec!["https://upload.iris.to".to_string()]
}

fn default_max_upload_mb() -> u64 {
    5
}

fn default_require_random_untrusted_ingest() -> bool {
    true
}

fn default_optimistic_uploads() -> bool {
    false
}

fn default_replicate_queue_mb() -> u64 {
    256
}

fn default_nostr_enabled() -> bool {
    true
}

fn default_blossom_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConfig {
    /// Enable background sync (auto-pull trees)
    #[serde(default = "default_sync_enabled")]
    pub enabled: bool,
    /// Sync own trees (subscribed via Nostr)
    #[serde(default = "default_sync_own")]
    pub sync_own: bool,
    /// Sync followed users' public trees
    #[serde(default = "default_sync_followed")]
    pub sync_followed: bool,
    /// Max concurrent sync tasks
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    /// WebRTC request timeout in milliseconds
    #[serde(default = "default_webrtc_timeout_ms")]
    pub webrtc_timeout_ms: u64,
    /// Blossom request timeout in milliseconds
    #[serde(default = "default_blossom_timeout_ms")]
    pub blossom_timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CashuConfig {
    /// Cashu mint base URLs we accept for bandwidth incentives.
    #[serde(default)]
    pub accepted_mints: Vec<String>,
    /// Default mint to use for wallet operations.
    #[serde(default)]
    pub default_mint: Option<String>,
    /// Default post-delivery payment offer for quoted retrievals.
    #[serde(default = "default_cashu_quote_payment_offer_sat")]
    pub quote_payment_offer_sat: u64,
    /// Quote validity window in milliseconds.
    #[serde(default = "default_cashu_quote_ttl_ms")]
    pub quote_ttl_ms: u32,
    /// Maximum time to wait for post-delivery settlement before recording a default.
    #[serde(default = "default_cashu_settlement_timeout_ms")]
    pub settlement_timeout_ms: u64,
    /// Block mints whose failed redemptions keep outnumbering successful redemptions.
    #[serde(default = "default_cashu_mint_failure_block_threshold")]
    pub mint_failure_block_threshold: u64,
    /// Base cap for trying a peer-suggested mint we do not already trust.
    #[serde(default = "default_cashu_peer_suggested_mint_base_cap_sat")]
    pub peer_suggested_mint_base_cap_sat: u64,
    /// Additional cap granted per successful delivery from that peer.
    #[serde(default = "default_cashu_peer_suggested_mint_success_step_sat")]
    pub peer_suggested_mint_success_step_sat: u64,
    /// Additional cap granted per settled payment received from that peer.
    #[serde(default = "default_cashu_peer_suggested_mint_receipt_step_sat")]
    pub peer_suggested_mint_receipt_step_sat: u64,
    /// Hard ceiling for untrusted peer-suggested mint exposure.
    #[serde(default = "default_cashu_peer_suggested_mint_max_cap_sat")]
    pub peer_suggested_mint_max_cap_sat: u64,
    /// Block serving peers whose unpaid defaults reach this threshold.
    #[serde(default)]
    pub payment_default_block_threshold: u64,
    /// Target chunk size for quoted paid delivery.
    #[serde(default = "default_cashu_chunk_target_bytes")]
    pub chunk_target_bytes: usize,
}

impl Default for CashuConfig {
    fn default() -> Self {
        Self {
            accepted_mints: Vec::new(),
            default_mint: None,
            quote_payment_offer_sat: default_cashu_quote_payment_offer_sat(),
            quote_ttl_ms: default_cashu_quote_ttl_ms(),
            settlement_timeout_ms: default_cashu_settlement_timeout_ms(),
            mint_failure_block_threshold: default_cashu_mint_failure_block_threshold(),
            peer_suggested_mint_base_cap_sat: default_cashu_peer_suggested_mint_base_cap_sat(),
            peer_suggested_mint_success_step_sat:
                default_cashu_peer_suggested_mint_success_step_sat(),
            peer_suggested_mint_receipt_step_sat:
                default_cashu_peer_suggested_mint_receipt_step_sat(),
            peer_suggested_mint_max_cap_sat: default_cashu_peer_suggested_mint_max_cap_sat(),
            payment_default_block_threshold: 0,
            chunk_target_bytes: default_cashu_chunk_target_bytes(),
        }
    }
}

fn default_cashu_quote_payment_offer_sat() -> u64 {
    3
}

fn default_cashu_quote_ttl_ms() -> u32 {
    1_500
}

fn default_cashu_settlement_timeout_ms() -> u64 {
    5_000
}

fn default_cashu_mint_failure_block_threshold() -> u64 {
    2
}

fn default_cashu_peer_suggested_mint_base_cap_sat() -> u64 {
    3
}

fn default_cashu_peer_suggested_mint_success_step_sat() -> u64 {
    1
}

fn default_cashu_peer_suggested_mint_receipt_step_sat() -> u64 {
    2
}

fn default_cashu_peer_suggested_mint_max_cap_sat() -> u64 {
    21
}

fn default_cashu_chunk_target_bytes() -> usize {
    32 * 1024
}

fn default_sync_enabled() -> bool {
    true
}

fn default_sync_own() -> bool {
    true
}

fn default_sync_followed() -> bool {
    true
}

fn default_max_concurrent() -> usize {
    3
}

fn default_webrtc_timeout_ms() -> u64 {
    2000
}

fn default_blossom_timeout_ms() -> u64 {
    10000
}

fn default_social_graph_crawl_depth() -> u32 {
    2
}

fn default_nostr_bootstrap_follows() -> Vec<String> {
    vec![hashtree_config::DEFAULT_SOCIALGRAPH_ENTRYPOINT_NPUB.to_string()]
}

fn default_max_write_distance() -> u32 {
    3
}

fn default_nostr_db_max_size_gb() -> u64 {
    10
}

fn default_nostr_spambox_max_size_gb() -> u64 {
    1
}

fn default_nostr_history_sync_on_reconnect() -> bool {
    true
}

fn default_nostr_overmute_threshold() -> f64 {
    1.0
}

fn default_nostr_mirror_kinds() -> Vec<u16> {
    vec![
        0, 1, 3, 5, 6, 7, 16, 20, 1_111, 9_735, 10_000, 30_000, 30_023,
    ]
}

fn default_nostr_history_sync_author_chunk_size() -> usize {
    5_000
}

fn default_nostr_history_sync_per_author_event_limit() -> usize {
    256
}

fn default_nostr_full_text_note_history_follow_distance() -> Option<u32> {
    Some(2)
}

fn default_nostr_full_text_note_history_max_relay_pages() -> usize {
    0
}

fn default_nostr_archive_history_follow_distance() -> Option<u32> {
    Some(2)
}

fn default_nostr_archive_history_max_relay_pages() -> usize {
    0
}

fn default_relays() -> Vec<String> {
    vec![
        "wss://nos.lol".to_string(),
        "wss://relay.snort.social".to_string(),
        "wss://temp.iris.to".to_string(),
    ]
}

fn default_bind_address() -> String {
    "127.0.0.1:8080".to_string()
}

fn default_enable_auth() -> bool {
    true
}

fn default_stun_port() -> u16 {
    3478 // Standard STUN port (RFC 5389)
}

fn default_enable_webrtc() -> bool {
    true
}

fn default_enable_fips() -> bool {
    true
}

fn default_enable_fips_lan_discovery() -> bool {
    true
}

fn default_fips_discovery_scope() -> String {
    hashtree_fips_transport::DEFAULT_FIPS_DISCOVERY_SCOPE.to_string()
}

fn default_enable_fips_udp() -> bool {
    true
}

fn default_enable_fips_webrtc() -> bool {
    cfg!(feature = "fips-webrtc")
}

fn default_fetch_from_fips_peers() -> bool {
    true
}

fn default_fips_request_timeout_ms() -> u64 {
    5_500
}

fn default_http_webrtc_fetch() -> bool {
    true
}

fn default_enable_multicast() -> bool {
    true
}

fn default_multicast_group() -> String {
    "239.255.42.98".to_string()
}

fn default_multicast_port() -> u16 {
    48555
}

fn default_max_multicast_peers() -> usize {
    12
}

fn default_enable_wifi_aware() -> bool {
    false
}

fn default_max_wifi_aware_peers() -> usize {
    0
}

fn default_enable_bluetooth() -> bool {
    false
}

fn default_max_bluetooth_peers() -> usize {
    0
}

fn default_data_dir() -> String {
    hashtree_config::get_hashtree_dir()
        .join("data")
        .to_string_lossy()
        .to_string()
}

fn default_max_size_gb() -> u64 {
    10
}

fn default_storage_evict_orphans() -> bool {
    true
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            mode: ServerMode::default(),
            bind_address: default_bind_address(),
            enable_auth: default_enable_auth(),
            stun_port: default_stun_port(),
            enable_webrtc: default_enable_webrtc(),
            enable_fips: default_enable_fips(),
            fips_discovery_scope: default_fips_discovery_scope(),
            fips_open_discovery_max_pending: 0,
            fips_local_rendezvous_addr: None,
            enable_fips_lan_discovery: default_enable_fips_lan_discovery(),
            fips_relays: None,
            fips_peers: Vec::new(),
            enable_fips_udp: default_enable_fips_udp(),
            fips_udp_bind_addr: None,
            fips_udp_public: false,
            fips_udp_external_addr: None,
            enable_fips_webrtc: default_enable_fips_webrtc(),
            fips_ethernet_interfaces: Vec::new(),
            fetch_from_fips_peers: default_fetch_from_fips_peers(),
            fips_request_timeout_ms: default_fips_request_timeout_ms(),
            http_webrtc_fetch: default_http_webrtc_fetch(),
            peer_signal_urls: Vec::new(),
            enable_multicast: default_enable_multicast(),
            multicast_group: default_multicast_group(),
            multicast_port: default_multicast_port(),
            max_multicast_peers: default_max_multicast_peers(),
            enable_wifi_aware: default_enable_wifi_aware(),
            max_wifi_aware_peers: default_max_wifi_aware_peers(),
            enable_bluetooth: default_enable_bluetooth(),
            max_bluetooth_peers: default_max_bluetooth_peers(),
            public_writes: default_public_writes(),
            public_plaintext_reads: default_public_plaintext_reads(),
            socialgraph_snapshot_public: default_socialgraph_snapshot_public(),
        }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            max_size_gb: default_max_size_gb(),
            evict_orphans: default_storage_evict_orphans(),
            s3: None,
        }
    }
}

impl Default for NostrConfig {
    fn default() -> Self {
        Self {
            enabled: default_nostr_enabled(),
            relays: default_relays(),
            event_transport: NostrEventTransport::default(),
            allowed_npubs: Vec::new(),
            socialgraph_root: None,
            bootstrap_follows: default_nostr_bootstrap_follows(),
            social_graph_crawl_depth: default_social_graph_crawl_depth(),
            mirror_max_follow_distance: None,
            max_write_distance: default_max_write_distance(),
            db_max_size_gb: default_nostr_db_max_size_gb(),
            spambox_max_size_gb: default_nostr_spambox_max_size_gb(),
            negentropy_only: false,
            overmute_threshold: default_nostr_overmute_threshold(),
            mirror_kinds: default_nostr_mirror_kinds(),
            history_sync_author_chunk_size: default_nostr_history_sync_author_chunk_size(),
            history_sync_per_author_event_limit: default_nostr_history_sync_per_author_event_limit(
            ),
            history_sync_on_reconnect: default_nostr_history_sync_on_reconnect(),
            full_text_note_history_follow_distance:
                default_nostr_full_text_note_history_follow_distance(),
            full_text_note_history_max_relay_pages:
                default_nostr_full_text_note_history_max_relay_pages(),
            archive_history_follow_distance: default_nostr_archive_history_follow_distance(),
            archive_history_max_relay_pages: default_nostr_archive_history_max_relay_pages(),
            decentralized_pubsub: false,
            decentralized_pubsub_max_event_bytes:
                default_nostr_decentralized_pubsub_max_event_bytes(),
        }
    }
}

impl Default for BlossomConfig {
    fn default() -> Self {
        Self {
            enabled: default_blossom_enabled(),
            servers: Vec::new(),
            read_servers: default_read_servers(),
            write_servers: default_write_servers(),
            max_upload_mb: default_max_upload_mb(),
            require_random_untrusted_ingest: default_require_random_untrusted_ingest(),
            optimistic_uploads: default_optimistic_uploads(),
            replicate_servers: Vec::new(),
            replicate_queue_mb: default_replicate_queue_mb(),
        }
    }
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            enabled: default_sync_enabled(),
            sync_own: default_sync_own(),
            sync_followed: default_sync_followed(),
            max_concurrent: default_max_concurrent(),
            webrtc_timeout_ms: default_webrtc_timeout_ms(),
            blossom_timeout_ms: default_blossom_timeout_ms(),
        }
    }
}

impl Config {
    /// Load config from file, or create default if doesn't exist
    pub fn load() -> Result<Self> {
        let config_path = get_config_path();

        if config_path.exists() {
            let content = fs::read_to_string(&config_path).context("Failed to read config file")?;
            toml::from_str(&content).context("Failed to parse config file")
        } else {
            let config = Config::default();
            config.save()?;
            Ok(config)
        }
    }

    /// Save config to file
    pub fn save(&self) -> Result<()> {
        let config_path = get_config_path();

        // Ensure parent directory exists
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let content = toml::to_string_pretty(self)?;
        fs::write(&config_path, content)?;

        Ok(())
    }
}

// Re-export path functions from hashtree_config
pub use hashtree_config::{get_auth_cookie_path, get_config_path, get_hashtree_dir, get_keys_path};

fn read_keys_from_path(keys_path: &Path) -> Result<Keys> {
    let content = fs::read_to_string(keys_path).context("Failed to read keys file")?;
    let entries = hashtree_config::parse_keys_file(&content);
    let nsec_str = entries
        .into_iter()
        .next()
        .map(|e| e.secret)
        .context("Keys file is empty")?;
    let secret_key = SecretKey::from_bech32(&nsec_str).context("Invalid nsec format")?;
    Ok(Keys::new(secret_key))
}

fn seed_identity_defaults_if_needed(data_dir: Option<&Path>, config: Option<&Config>) {
    if let (Some(data_dir), Some(config)) = (data_dir, config) {
        let _ = crate::bootstrap::seed_identity_defaults(data_dir, config);
    }
}

fn write_keys_to_path(keys_path: &Path, keys: &Keys) -> Result<()> {
    if let Some(parent) = keys_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let nsec = keys
        .secret_key()
        .to_bech32()
        .context("Failed to encode nsec")?;
    fs::write(keys_path, &nsec)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(keys_path, perms)?;
    }

    Ok(())
}

/// Generate and save auth cookie if it doesn't exist
pub fn ensure_auth_cookie() -> Result<(String, String)> {
    let cookie_path = get_auth_cookie_path();

    if cookie_path.exists() {
        read_auth_cookie()
    } else {
        generate_auth_cookie()
    }
}

/// Read existing auth cookie
pub fn read_auth_cookie() -> Result<(String, String)> {
    let cookie_path = get_auth_cookie_path();
    let content = fs::read_to_string(&cookie_path).context("Failed to read auth cookie")?;

    let parts: Vec<&str> = content.trim().split(':').collect();
    if parts.len() != 2 {
        anyhow::bail!("Invalid auth cookie format");
    }

    Ok((parts[0].to_string(), parts[1].to_string()))
}

/// Ensure keys file exists, generating one if not present
/// Returns (Keys, was_generated)
pub fn ensure_keys() -> Result<(Keys, bool)> {
    let config_dir = get_hashtree_dir();
    let config = Config::load().ok();
    let data_dir = config
        .as_ref()
        .map(|cfg| Path::new(cfg.storage.data_dir.as_str()));
    ensure_keys_in(&config_dir, data_dir, config.as_ref())
}

/// Ensure keys exist inside an explicit config directory.
/// Returns (Keys, was_generated)
pub fn ensure_keys_in(
    config_dir: &Path,
    data_dir: Option<&Path>,
    config: Option<&Config>,
) -> Result<(Keys, bool)> {
    let keys_path = config_dir.join("keys");

    if keys_path.exists() {
        Ok((read_keys_from_path(&keys_path)?, false))
    } else {
        let keys = generate_keys_in(config_dir, data_dir, config)?;
        Ok((keys, true))
    }
}

/// Read existing keys
pub fn read_keys() -> Result<Keys> {
    read_keys_in(&get_hashtree_dir())
}

/// Read keys from an explicit config directory.
pub fn read_keys_in(config_dir: &Path) -> Result<Keys> {
    read_keys_from_path(&config_dir.join("keys"))
}

/// Get nsec string, ensuring keys file exists (generate if needed)
/// Returns (nsec_string, was_generated)
pub fn ensure_keys_string() -> Result<(String, bool)> {
    let config_dir = get_hashtree_dir();
    let config = Config::load().ok();
    let data_dir = config
        .as_ref()
        .map(|cfg| Path::new(cfg.storage.data_dir.as_str()));
    ensure_keys_string_in(&config_dir, data_dir, config.as_ref())
}

/// Ensure key material exists inside an explicit config directory.
/// Returns (nsec_string, was_generated)
pub fn ensure_keys_string_in(
    config_dir: &Path,
    data_dir: Option<&Path>,
    config: Option<&Config>,
) -> Result<(String, bool)> {
    let keys_path = config_dir.join("keys");

    if keys_path.exists() {
        let content = fs::read_to_string(&keys_path).context("Failed to read keys file")?;
        let entries = hashtree_config::parse_keys_file(&content);
        let nsec_str = entries
            .into_iter()
            .next()
            .map(|e| e.secret)
            .context("Keys file is empty")?;
        Ok((nsec_str, false))
    } else {
        let keys = generate_keys_in(config_dir, data_dir, config)?;
        let nsec = keys
            .secret_key()
            .to_bech32()
            .context("Failed to encode nsec")?;
        Ok((nsec, true))
    }
}

/// Generate new keys and save to file
pub fn generate_keys() -> Result<Keys> {
    let config_dir = get_hashtree_dir();
    let config = Config::load().ok();
    let data_dir = config
        .as_ref()
        .map(|cfg| Path::new(cfg.storage.data_dir.as_str()));
    generate_keys_in(&config_dir, data_dir, config.as_ref())
}

/// Generate new keys in an explicit config directory and optionally seed
/// identity defaults into a caller-owned data directory.
pub fn generate_keys_in(
    config_dir: &Path,
    data_dir: Option<&Path>,
    config: Option<&Config>,
) -> Result<Keys> {
    let keys = Keys::generate();
    write_keys_to_path(&config_dir.join("keys"), &keys)?;
    seed_identity_defaults_if_needed(data_dir, config);
    Ok(keys)
}

/// Get 32-byte pubkey bytes from Keys.
pub fn pubkey_bytes(keys: &Keys) -> [u8; 32] {
    keys.public_key().to_bytes()
}

/// Parse npub to 32-byte pubkey
pub fn parse_npub(npub: &str) -> Result<[u8; 32]> {
    use nostr::PublicKey;
    let pk = PublicKey::from_bech32(npub).context("Invalid npub format")?;
    Ok(pk.to_bytes())
}

/// Generate new random auth cookie
pub fn generate_auth_cookie() -> Result<(String, String)> {
    use rand::Rng;

    let cookie_path = get_auth_cookie_path();

    // Ensure parent directory exists
    if let Some(parent) = cookie_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Generate random credentials
    let mut rng = rand::thread_rng();
    let username = format!("htree_{}", rng.gen::<u32>());
    let password: String = (0..32)
        .map(|_| {
            let idx = rng.gen_range(0..62);
            match idx {
                0..=25 => (b'a' + idx) as char,
                26..=51 => (b'A' + (idx - 26)) as char,
                _ => (b'0' + (idx - 52)) as char,
            }
        })
        .collect();

    // Save to file
    let content = format!("{}:{}", username, password);
    fs::write(&cookie_path, content)?;

    // Set permissions to 0600 (owner read/write only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(&cookie_path, perms)?;
    }

    Ok((username, password))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{test_env_lock, EnvVarGuard};
    use tempfile::TempDir;

    #[test]
    fn test_config_default() {
        let config = Config::default();
        assert_eq!(config.server.bind_address, "127.0.0.1:8080");
        assert!(config.server.enable_auth);
        assert!(config.server.enable_multicast);
        assert_eq!(config.server.multicast_group, "239.255.42.98");
        assert_eq!(config.server.multicast_port, 48555);
        assert_eq!(config.server.max_multicast_peers, 12);
        assert!(!config.server.enable_wifi_aware);
        assert_eq!(config.server.max_wifi_aware_peers, 0);
        assert!(!config.server.enable_bluetooth);
        assert_eq!(config.server.max_bluetooth_peers, 0);
        assert!(!config.server.public_plaintext_reads);
        assert_eq!(config.storage.max_size_gb, 10);
        assert!(config.storage.evict_orphans);
        assert!(config.nostr.enabled);
        assert!(config
            .nostr
            .relays
            .contains(&"wss://temp.iris.to".to_string()));
        assert!(config.blossom.enabled);
        assert!(!config.blossom.optimistic_uploads);
        assert!(config.blossom.replicate_servers.is_empty());
        assert_eq!(config.blossom.replicate_queue_mb, 256);
        assert_eq!(config.nostr.social_graph_crawl_depth, 2);
        assert_eq!(config.nostr.mirror_max_follow_distance, None);
        assert_eq!(config.nostr.max_write_distance, 3);
        assert_eq!(config.nostr.db_max_size_gb, 10);
        assert_eq!(config.nostr.spambox_max_size_gb, 1);
        assert!(!config.nostr.negentropy_only);
        assert_eq!(config.nostr.overmute_threshold, 1.0);
        assert_eq!(
            config.nostr.mirror_kinds,
            vec![0, 1, 3, 5, 6, 7, 16, 20, 1_111, 9_735, 10_000, 30_000, 30_023]
        );
        assert_eq!(config.nostr.history_sync_author_chunk_size, 5_000);
        assert_eq!(config.nostr.history_sync_per_author_event_limit, 256);
        assert!(config.nostr.history_sync_on_reconnect);
        assert_eq!(config.nostr.full_text_note_history_follow_distance, Some(2));
        assert_eq!(config.nostr.full_text_note_history_max_relay_pages, 0);
        assert_eq!(config.nostr.archive_history_follow_distance, Some(2));
        assert_eq!(config.nostr.archive_history_max_relay_pages, 0);
        assert!(config.nostr.socialgraph_root.is_none());
        assert_eq!(
            config.nostr.bootstrap_follows,
            vec![hashtree_config::DEFAULT_SOCIALGRAPH_ENTRYPOINT_NPUB.to_string()]
        );
        assert!(!config.server.socialgraph_snapshot_public);
        assert!(config.cashu.accepted_mints.is_empty());
        assert!(config.cashu.default_mint.is_none());
        assert_eq!(config.cashu.quote_payment_offer_sat, 3);
        assert_eq!(config.cashu.quote_ttl_ms, 1_500);
        assert_eq!(config.cashu.settlement_timeout_ms, 5_000);
        assert_eq!(config.cashu.mint_failure_block_threshold, 2);
        assert_eq!(config.cashu.peer_suggested_mint_base_cap_sat, 3);
        assert_eq!(config.cashu.peer_suggested_mint_success_step_sat, 1);
        assert_eq!(config.cashu.peer_suggested_mint_receipt_step_sat, 2);
        assert_eq!(config.cashu.peer_suggested_mint_max_cap_sat, 21);
        assert_eq!(config.cashu.payment_default_block_threshold, 0);
        assert_eq!(config.cashu.chunk_target_bytes, 32 * 1024);
    }

    #[test]
    fn test_blossom_optimistic_uploads_deserialize() {
        let toml_str = r#"
[blossom]
optimistic_uploads = true
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.blossom.optimistic_uploads);
        assert!(config.blossom.require_random_untrusted_ingest);
    }

    #[test]
    fn test_blossom_replication_deserialize() {
        let toml_str = r#"
[blossom]
replicate_servers = ["http://127.0.0.1:8081"]
replicate_queue_mb = 128
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.blossom.replicate_servers, ["http://127.0.0.1:8081"]);
        assert_eq!(config.blossom.replicate_queue_mb, 128);
    }

    #[test]
    fn test_server_public_plaintext_reads_deserialize() {
        let toml_str = r#"
[server]
public_plaintext_reads = false
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(!config.server.public_plaintext_reads);
        assert!(config.server.public_writes);
    }

    #[test]
    fn test_nostr_config_deserialize_with_defaults() {
        let toml_str = r#"
[nostr]
relays = ["wss://relay.damus.io"]
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.nostr.enabled);
        assert_eq!(config.nostr.relays, vec!["wss://relay.damus.io"]);
        assert!(config.storage.evict_orphans);
        assert_eq!(config.nostr.social_graph_crawl_depth, 2);
        assert_eq!(config.nostr.mirror_max_follow_distance, None);
        assert_eq!(config.nostr.max_write_distance, 3);
        assert_eq!(config.nostr.db_max_size_gb, 10);
        assert_eq!(config.nostr.spambox_max_size_gb, 1);
        assert!(!config.nostr.negentropy_only);
        assert_eq!(config.nostr.overmute_threshold, 1.0);
        assert_eq!(
            config.nostr.mirror_kinds,
            vec![0, 1, 3, 5, 6, 7, 16, 20, 1_111, 9_735, 10_000, 30_000, 30_023]
        );
        assert_eq!(config.nostr.history_sync_author_chunk_size, 5_000);
        assert_eq!(config.nostr.history_sync_per_author_event_limit, 256);
        assert!(config.nostr.history_sync_on_reconnect);
        assert_eq!(config.nostr.full_text_note_history_follow_distance, Some(2));
        assert_eq!(config.nostr.full_text_note_history_max_relay_pages, 0);
        assert_eq!(config.nostr.archive_history_follow_distance, Some(2));
        assert_eq!(config.nostr.archive_history_max_relay_pages, 0);
        assert!(config.nostr.socialgraph_root.is_none());
        assert_eq!(
            config.nostr.bootstrap_follows,
            vec![hashtree_config::DEFAULT_SOCIALGRAPH_ENTRYPOINT_NPUB.to_string()]
        );
    }

    #[test]
    fn test_nostr_config_deserialize_with_socialgraph() {
        let toml_str = r#"
[nostr]
relays = ["wss://relay.damus.io"]
socialgraph_root = "npub1test"
bootstrap_follows = []
social_graph_crawl_depth = 3
mirror_max_follow_distance = 2
max_write_distance = 5
negentropy_only = true
overmute_threshold = 2.5
mirror_kinds = [0, 10000]
history_sync_author_chunk_size = 250
history_sync_per_author_event_limit = 128
history_sync_on_reconnect = false
full_text_note_history_follow_distance = 1
full_text_note_history_max_relay_pages = 64
archive_history_follow_distance = 2
archive_history_max_relay_pages = 32
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.nostr.enabled);
        assert!(config.storage.evict_orphans);
        assert_eq!(config.nostr.socialgraph_root, Some("npub1test".to_string()));
        assert!(config.nostr.bootstrap_follows.is_empty());
        assert_eq!(config.nostr.social_graph_crawl_depth, 3);
        assert_eq!(config.nostr.mirror_max_follow_distance, Some(2));
        assert_eq!(config.nostr.max_write_distance, 5);
        assert_eq!(config.nostr.db_max_size_gb, 10);
        assert_eq!(config.nostr.spambox_max_size_gb, 1);
        assert!(config.nostr.negentropy_only);
        assert_eq!(config.nostr.overmute_threshold, 2.5);
        assert_eq!(config.nostr.mirror_kinds, vec![0, 10_000]);
        assert_eq!(config.nostr.history_sync_author_chunk_size, 250);
        assert_eq!(config.nostr.history_sync_per_author_event_limit, 128);
        assert!(!config.nostr.history_sync_on_reconnect);
        assert_eq!(config.nostr.full_text_note_history_follow_distance, Some(1));
        assert_eq!(config.nostr.full_text_note_history_max_relay_pages, 64);
        assert_eq!(config.nostr.archive_history_follow_distance, Some(2));
        assert_eq!(config.nostr.archive_history_max_relay_pages, 32);
    }

    #[test]
    fn test_nostr_config_deserialize_legacy_crawl_depth_alias() {
        let toml_str = r#"
[nostr]
relays = ["wss://relay.damus.io"]
crawl_depth = 4
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.nostr.social_graph_crawl_depth, 4);
    }

    #[test]
    fn test_storage_config_disables_orphan_eviction_when_requested() {
        let toml_str = r#"
[storage]
evict_orphans = false
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(!config.storage.evict_orphans);
    }

    #[test]
    fn test_server_config_deserialize_with_multicast() {
        let toml_str = r#"
[server]
enable_multicast = true
multicast_group = "239.255.42.99"
multicast_port = 49001
max_multicast_peers = 12
enable_wifi_aware = true
max_wifi_aware_peers = 5
enable_bluetooth = true
max_bluetooth_peers = 6
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.server.enable_multicast);
        assert_eq!(config.server.multicast_group, "239.255.42.99");
        assert_eq!(config.server.multicast_port, 49_001);
        assert_eq!(config.server.max_multicast_peers, 12);
        assert!(config.server.enable_wifi_aware);
        assert_eq!(config.server.max_wifi_aware_peers, 5);
        assert!(config.server.enable_bluetooth);
        assert_eq!(config.server.max_bluetooth_peers, 6);
    }

    #[test]
    fn test_cashu_config_deserialize_with_accepted_mints() {
        let toml_str = r#"
[cashu]
accepted_mints = ["https://mint1.example", "http://127.0.0.1:3338"]
default_mint = "https://mint1.example"
quote_payment_offer_sat = 5
quote_ttl_ms = 2500
settlement_timeout_ms = 7000
mint_failure_block_threshold = 3
peer_suggested_mint_base_cap_sat = 4
peer_suggested_mint_success_step_sat = 2
peer_suggested_mint_receipt_step_sat = 3
peer_suggested_mint_max_cap_sat = 34
payment_default_block_threshold = 2
chunk_target_bytes = 65536
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.cashu.accepted_mints,
            vec![
                "https://mint1.example".to_string(),
                "http://127.0.0.1:3338".to_string()
            ]
        );
        assert_eq!(
            config.cashu.default_mint,
            Some("https://mint1.example".to_string())
        );
        assert_eq!(config.cashu.quote_payment_offer_sat, 5);
        assert_eq!(config.cashu.quote_ttl_ms, 2500);
        assert_eq!(config.cashu.settlement_timeout_ms, 7_000);
        assert_eq!(config.cashu.mint_failure_block_threshold, 3);
        assert_eq!(config.cashu.peer_suggested_mint_base_cap_sat, 4);
        assert_eq!(config.cashu.peer_suggested_mint_success_step_sat, 2);
        assert_eq!(config.cashu.peer_suggested_mint_receipt_step_sat, 3);
        assert_eq!(config.cashu.peer_suggested_mint_max_cap_sat, 34);
        assert_eq!(config.cashu.payment_default_block_threshold, 2);
        assert_eq!(config.cashu.chunk_target_bytes, 65_536);
    }

    #[test]
    fn test_auth_cookie_generation() -> Result<()> {
        let _lock = test_env_lock().blocking_lock();
        let temp_dir = TempDir::new()?;
        let _guard = EnvVarGuard::set("HTREE_CONFIG_DIR", temp_dir.path());

        let (username, password) = generate_auth_cookie()?;

        assert!(username.starts_with("htree_"));
        assert_eq!(password.len(), 32);

        // Verify cookie file exists
        let cookie_path = get_auth_cookie_path();
        assert!(cookie_path.exists());

        // Verify reading works
        let (u2, p2) = read_auth_cookie()?;
        assert_eq!(username, u2);
        assert_eq!(password, p2);

        Ok(())
    }

    #[test]
    fn test_blossom_read_servers_include_write_only_servers_as_fresh_fallbacks() {
        let config = BlossomConfig {
            servers: vec!["https://legacy.server".to_string()],
            ..BlossomConfig::default()
        };

        let read = config.all_read_servers();
        assert!(read.contains(&"https://legacy.server".to_string()));
        assert!(read.contains(&"https://cdn.iris.to".to_string()));
        assert!(read.contains(&"https://blossom.primal.net".to_string()));
        assert!(read.contains(&"https://upload.iris.to".to_string()));

        let write = config.all_write_servers();
        assert!(write.contains(&"https://legacy.server".to_string()));
        assert!(write.contains(&"https://upload.iris.to".to_string()));
    }

    #[test]
    fn daemon_blossom_upstreams_exclude_its_own_loopback_http_endpoint() {
        let config = BlossomConfig {
            servers: Vec::new(),
            read_servers: vec![
                "http://127.0.0.1:19092".to_string(),
                "http://localhost:19092/".to_string(),
                "http://127.0.0.1:19093".to_string(),
                "https://127.0.0.1:19092".to_string(),
                "https://read.example".to_string(),
            ],
            write_servers: Vec::new(),
            ..BlossomConfig::default()
        };

        let upstreams = config.upstream_read_servers("127.0.0.1:19092");

        assert!(!upstreams
            .iter()
            .any(|server| server == "http://127.0.0.1:19092"));
        assert!(!upstreams
            .iter()
            .any(|server| server == "http://localhost:19092/"));
        assert!(upstreams
            .iter()
            .any(|server| server == "http://127.0.0.1:19093"));
        assert!(upstreams
            .iter()
            .any(|server| server == "https://127.0.0.1:19092"));
        assert!(upstreams
            .iter()
            .any(|server| server == "https://read.example"));
    }

    #[test]
    fn wildcard_daemon_bind_excludes_loopback_self_but_keeps_remote_upstreams() {
        let config = BlossomConfig {
            servers: Vec::new(),
            read_servers: vec![
                "http://localhost:8080".to_string(),
                "http://[::1]:8080".to_string(),
                "http://192.0.2.10:8080".to_string(),
            ],
            write_servers: Vec::new(),
            ..BlossomConfig::default()
        };

        let upstreams = config.upstream_read_servers("0.0.0.0:8080");

        assert!(!upstreams
            .iter()
            .any(|server| server == "http://localhost:8080"));
        assert!(!upstreams.iter().any(|server| server == "http://[::1]:8080"));
        assert!(upstreams
            .iter()
            .any(|server| server == "http://192.0.2.10:8080"));
    }

    #[test]
    fn test_blossom_servers_fall_back_to_defaults_when_explicitly_empty() {
        let config = BlossomConfig {
            enabled: true,
            servers: Vec::new(),
            read_servers: Vec::new(),
            write_servers: Vec::new(),
            max_upload_mb: default_max_upload_mb(),
            require_random_untrusted_ingest: default_require_random_untrusted_ingest(),
            optimistic_uploads: default_optimistic_uploads(),
            replicate_servers: Vec::new(),
            replicate_queue_mb: default_replicate_queue_mb(),
        };

        let read = config.all_read_servers();
        let mut expected = default_read_servers();
        expected.extend(default_write_servers());
        expected.sort();
        expected.dedup();
        assert_eq!(read, expected);

        let write = config.all_write_servers();
        assert_eq!(write, default_write_servers());
    }

    #[test]
    fn test_disabled_sources_preserve_lists_but_return_no_active_endpoints() {
        let nostr = NostrConfig {
            enabled: false,
            relays: vec!["wss://relay.example".to_string()],
            ..NostrConfig::default()
        };
        assert!(nostr.active_relays().is_empty());

        let blossom = BlossomConfig {
            enabled: false,
            servers: vec!["https://legacy.server".to_string()],
            read_servers: vec!["https://read.example".to_string()],
            write_servers: vec!["https://write.example".to_string()],
            max_upload_mb: default_max_upload_mb(),
            require_random_untrusted_ingest: default_require_random_untrusted_ingest(),
            optimistic_uploads: default_optimistic_uploads(),
            replicate_servers: Vec::new(),
            replicate_queue_mb: default_replicate_queue_mb(),
        };
        assert!(blossom.all_read_servers().is_empty());
        assert!(blossom.all_write_servers().is_empty());
    }

    #[test]
    fn fips_local_only_event_transport_never_exposes_direct_relays() {
        let nostr = NostrConfig {
            relays: vec!["wss://must-not-open.example".to_string()],
            event_transport: NostrEventTransport::FipsLocalOnly,
            ..NostrConfig::default()
        };

        assert!(nostr.active_relays().is_empty());
    }

    #[test]
    fn nostr_decentralized_pubsub_requires_config_and_feature() {
        let default_nostr = NostrConfig::default();
        assert!(!default_nostr.decentralized_pubsub);
        assert!(!default_nostr.decentralized_pubsub_enabled());

        let enabled: NostrConfig = toml::from_str("decentralized_pubsub = true")
            .expect("parse decentralized pubsub nostr config");
        assert!(enabled.decentralized_pubsub);
        assert_eq!(
            enabled.decentralized_pubsub_max_event_bytes,
            nostr_pubsub_fips::FIPS_NOSTR_PUBSUB_MAX_DATAGRAM_BYTES
        );
        assert_eq!(
            enabled.decentralized_pubsub_enabled(),
            cfg!(feature = "experimental-decentralized-pubsub")
        );

        let disabled: NostrConfig = toml::from_str(
            r#"
enabled = false
decentralized_pubsub = true
"#,
        )
        .expect("parse disabled decentralized pubsub nostr config");
        assert!(!disabled.decentralized_pubsub_enabled());

        let alias: NostrConfig =
            toml::from_str("relayless_pubsub = true").expect("parse compatibility pubsub alias");
        assert!(alias.decentralized_pubsub);

        let tuned: NostrConfig = toml::from_str(
            r#"
decentralized_pubsub = true
decentralized_pubsub_max_event_bytes = 4096
"#,
        )
        .expect("parse tuned decentralized pubsub config");
        assert_eq!(tuned.decentralized_pubsub_max_event_bytes, 4096);
    }

    #[test]
    fn server_defaults_enable_fips_udp_and_feature_gated_webrtc() {
        let server = ServerConfig::default();

        assert!(server.enable_fips);
        assert!(server.enable_fips_udp);
        assert!(server.fips_udp_bind_addr.is_none());
        assert!(!server.fips_udp_public);
        assert!(server.fips_udp_external_addr.is_none());
        assert_eq!(server.enable_fips_webrtc, cfg!(feature = "fips-webrtc"));
        assert!(server.fips_ethernet_interfaces.is_empty());
        assert!(server.fetch_from_fips_peers);
        assert!(server.fips_relays.is_none());
        assert!(server.fips_peers.is_empty());
        assert_eq!(server.fips_discovery_scope, "fips-overlay-v1");
        assert_eq!(server.fips_open_discovery_max_pending, 0);
        assert!(server.fips_local_rendezvous_addr.is_none());
        assert!(server.enable_fips_lan_discovery);
        assert_eq!(server.fips_request_timeout_ms, 5_500);
    }

    #[test]
    fn server_config_reads_fips_overrides() {
        let config: Config = toml::from_str(
            r#"
[server]
enable_fips = true
fips_discovery_scope = "test-hashtree"
fips_open_discovery_max_pending = 32
fips_local_rendezvous_addr = "127.0.0.1:32111"
enable_fips_lan_discovery = false
fips_relays = ["wss://fips.example"]
fips_peers = [
  { npub = "npub1origin", udp_addresses = ["udp:192.0.2.10:2121"] },
  { npub = "npub1cache" },
]
enable_fips_udp = false
fips_udp_bind_addr = "0.0.0.0:2121"
fips_udp_public = true
fips_udp_external_addr = "198.19.77.10:2121"
enable_fips_webrtc = true
fips_ethernet_interfaces = ["eth0"]
fetch_from_fips_peers = false
fips_request_timeout_ms = 42
"#,
        )
        .unwrap();

        assert!(config.server.enable_fips);
        assert_eq!(config.server.fips_discovery_scope, "test-hashtree");
        assert_eq!(config.server.fips_open_discovery_max_pending, 32);
        assert_eq!(
            config.server.fips_local_rendezvous_addr.as_deref(),
            Some("127.0.0.1:32111")
        );
        assert!(!config.server.enable_fips_lan_discovery);
        assert_eq!(
            config.server.fips_relays,
            Some(vec!["wss://fips.example".to_string()])
        );
        assert_eq!(
            config.server.fips_peers,
            [
                ConfiguredFipsPeer {
                    npub: "npub1origin".to_string(),
                    udp_addresses: vec!["udp:192.0.2.10:2121".to_string()],
                },
                ConfiguredFipsPeer {
                    npub: "npub1cache".to_string(),
                    udp_addresses: Vec::new(),
                },
            ]
        );
        assert!(!config.server.enable_fips_udp);
        assert_eq!(
            config.server.fips_udp_bind_addr.as_deref(),
            Some("0.0.0.0:2121")
        );
        assert!(config.server.fips_udp_public);
        assert_eq!(
            config.server.fips_udp_external_addr.as_deref(),
            Some("198.19.77.10:2121")
        );
        assert!(config.server.enable_fips_webrtc);
        assert_eq!(config.server.fips_ethernet_interfaces, ["eth0"]);
        assert!(!config.server.fetch_from_fips_peers);
        assert_eq!(config.server.fips_request_timeout_ms, 42);
    }

    #[test]
    fn server_config_accepts_legacy_http_fips_fetch_name() {
        let config: Config = toml::from_str(
            r#"
[server]
http_fips_fetch = false
"#,
        )
        .unwrap();

        assert!(!config.server.fetch_from_fips_peers);
    }

    #[test]
    fn fips_relay_resolution_prefers_fips_relays_then_nostr() {
        let active_nostr = vec!["wss://nostr.example".to_string()];
        let mut server = ServerConfig::default();

        assert_eq!(
            server.resolved_fips_relays(&active_nostr),
            [
                "wss://nostr.example",
                "wss://temp.iris.to",
                "wss://relay.primal.net"
            ]
        );

        server.fips_relays = Some(vec!["wss://fips.example".to_string()]);
        assert_eq!(
            server.resolved_fips_relays(&["wss://ignored.example".to_string()]),
            ["wss://fips.example"]
        );
    }

    #[test]
    fn explicit_fips_relay_resolution_is_exact_and_normalized() {
        let server = ServerConfig {
            fips_relays: Some(vec![
                "wss://temp.iris.to/".to_string(),
                " wss://relay.primal.net ".to_string(),
                "wss://temp.iris.to".to_string(),
                "wss://extra.example".to_string(),
            ]),
            ..ServerConfig::default()
        };

        assert_eq!(
            server.resolved_fips_relays(&[]),
            [
                "wss://temp.iris.to",
                "wss://relay.primal.net",
                "wss://extra.example"
            ]
        );
    }

    #[test]
    fn explicit_empty_fips_relays_disable_all_relay_discovery() {
        let config: Config = toml::from_str(
            r#"
[server]
enable_fips = true
enable_fips_udp = false
enable_fips_webrtc = false
fips_ethernet_interfaces = ["eth0"]
fips_relays = []

[nostr]
relays = ["wss://must-not-open.example"]
event_transport = "fips-local-only"
"#,
        )
        .unwrap();

        assert_eq!(config.server.fips_relays, Some(Vec::new()));
        assert_eq!(
            config.nostr.event_transport,
            NostrEventTransport::FipsLocalOnly
        );
        assert!(config.nostr.active_relays().is_empty());
        assert!(config
            .server
            .resolved_fips_relays(&["wss://must-not-open.example".to_string()])
            .is_empty());
    }
}
