//! `[paid_retrieval]` in the daemon's configuration.
//!
//! Nothing here has a default that costs or earns money: a section that is
//! absent starts nothing, and a section that is present must name its prices,
//! mints and caps itself.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use hashtree_fips_transport::PeerIdentity;
use hashtree_paid_retrieval::{BytePrice, ClientPolicy, LockKey, ProviderConfig, Tariff};
use serde::{Deserialize, Serialize};

/// Where the daemon keeps what paid retrieval must remember, in its data
/// directory: the provider ledger, the client accounts and the spent total.
pub const PAID_RETRIEVAL_DIR: &str = "paid-retrieval";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaidRetrievalConfig {
    /// Sell blobs from the local store. Absent: nothing is sold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<PaidProviderConfig>,
    /// Buy blobs that no free route returned. Absent: nothing is bought.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<PaidClientConfig>,
}

impl PaidRetrievalConfig {
    pub fn is_unset(&self) -> bool {
        self.provider.is_none() && self.client.is_none()
    }

    /// Whether the free blob service must stop answering peers.
    pub fn paying_only(&self) -> bool {
        self.provider
            .as_ref()
            .is_some_and(|provider| provider.paying_only)
    }

    /// Identities of the providers the client buys from.
    pub fn client_provider_npubs(&self) -> impl Iterator<Item = &str> {
        let providers = self.client.iter().flat_map(|client| &client.providers);
        providers.filter_map(|provider| provider.npub.as_deref())
    }
}

/// `msat` for every `per_bytes` bytes, and at least `min_msat` per blob.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaidTariffConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msat: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_msat: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaidProviderConfig {
    /// What a blob costs. Required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tariff: Option<PaidTariffConfig>,
    /// Mints whose tokens are accepted. Required: at least one.
    #[serde(default)]
    pub mints: Vec<String>,
    /// Smallest top-up accepted, in sat.
    #[serde(default = "smallest_topup_sat")]
    pub min_topup_sat: u64,
    /// Msat served before payment to an authenticated peer. Zero is prepay.
    #[serde(default)]
    pub grace_msat: u64,
    /// Directory of the ledger. Default: `paid-retrieval/ledger` in the data
    /// directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger_dir: Option<String>,
    /// Stop answering peers on the free blob service, so that a blob in the
    /// local store reaches them only through the paid one.
    #[serde(default)]
    pub paying_only: bool,
    /// Also sell through the HTTP listener, at `POST /paid-blob`. A buyer
    /// there is not authenticated and gets no grace.
    #[serde(default)]
    pub http: bool,
    /// A public key (33 bytes, hex) that tokens should be locked to, named
    /// in every quote. Unlocked tokens are accepted as well. Unset: none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock: Option<String>,
}

/// A Cashu token is whole sats, so one sat is the smallest top-up there is.
fn smallest_topup_sat() -> u64 {
    1
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaidClientConfig {
    /// The most this daemon may spend on retrieval in total, in sat. Required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spending_limit_sat: Option<u64>,
    /// The highest price accepted. Required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tariff: Option<PaidTariffConfig>,
    /// Mints the wallet may pay with. Required: at least one.
    #[serde(default)]
    pub mints: Vec<String>,
    /// Size of one top-up, in sat. Required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topup_sat: Option<u64>,
    /// The most one provider may hold in advance, in msat. Required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_prepaid_msat: Option<u64>,
    /// Loss after which a provider is no longer paid, in msat. Required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_loss_msat: Option<u64>,
    /// The most that creating one token may cost, in sat. Required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_token_fee_sat: Option<u64>,
    /// The most a provider may keep of one token for redeeming it, in msat.
    /// Required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_redeem_fee_msat: Option<u64>,
    /// Also pay for reads when the HTTP listener is not bound to loopback,
    /// that is for whoever can reach it.
    #[serde(default)]
    pub pay_for_remote_reads: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<PaidProviderPeerConfig>,
}

/// One provider to buy from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaidProviderPeerConfig {
    /// The operator's name for it, shown in the status and to the wallet.
    pub name: String,
    /// Its FIPS identity. Set this or `url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npub: Option<String>,
    /// The base URL of its HTTP gateway. Set this or `npub`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// A validated `[paid_retrieval.provider]`.
#[derive(Debug, Clone)]
pub struct ProviderSettings {
    pub config: ProviderConfig,
    pub ledger_dir: PathBuf,
    pub paying_only: bool,
    pub http: bool,
}

/// Where a provider of the client is reached.
#[derive(Debug, Clone)]
pub enum ProviderPlace {
    Fips {
        npub: String,
        identity: PeerIdentity,
    },
    Http {
        url: reqwest::Url,
    },
}

impl ProviderPlace {
    /// What the status shows and what names the saved account: the npub, or
    /// the gateway's endpoint URL.
    pub fn label(&self) -> String {
        match self {
            Self::Fips { npub, .. } => npub.clone(),
            Self::Http { url } => url.to_string(),
        }
    }
}

/// One validated provider of the client.
#[derive(Debug, Clone)]
pub struct ProviderPeer {
    pub name: String,
    pub place: ProviderPlace,
}

/// A validated `[paid_retrieval.client]`.
#[derive(Debug, Clone)]
pub struct ClientSettings {
    pub policy: ClientPolicy,
    pub spending_limit_sat: u64,
    pub pay_for_remote_reads: bool,
    pub providers: Vec<ProviderPeer>,
    /// Holds the spent total and one account file per provider.
    pub state_dir: PathBuf,
}

/// What the configuration asks the daemon to start.
#[derive(Debug, Clone)]
pub struct PaidRetrievalSettings {
    pub provider: Option<ProviderSettings>,
    pub client: Option<ClientSettings>,
}

impl PaidRetrievalSettings {
    /// `None` when the configuration asks for nothing. A section that is
    /// present and incomplete is an error, never a silent default.
    pub fn from_config(config: &PaidRetrievalConfig, data_dir: &Path) -> Result<Option<Self>> {
        let state_dir = data_dir.join(PAID_RETRIEVAL_DIR);
        let provider = match &config.provider {
            Some(provider) => Some(provider_settings(provider, &state_dir)?),
            None => None,
        };
        let client = match &config.client {
            // A policy without anyone to buy from registers nothing.
            Some(client) if !client.providers.is_empty() => {
                Some(client_settings(client, state_dir)?)
            }
            _ => None,
        };
        Ok(match (&provider, &client) {
            (None, None) => None,
            _ => Some(Self { provider, client }),
        })
    }
}

/// Collects every missing value of one section, to name them all at once.
struct Required<'a> {
    section: &'a str,
    missing: Vec<String>,
}

impl<'a> Required<'a> {
    fn new(section: &'a str) -> Self {
        Self {
            section,
            missing: Vec::new(),
        }
    }

    fn value(&mut self, key: &str, value: Option<u64>) -> u64 {
        if value.is_none() {
            self.missing.push(key.to_string());
        }
        value.unwrap_or_default()
    }

    fn tariff(&mut self, key: &str, tariff: Option<&PaidTariffConfig>) -> Tariff {
        let unset = PaidTariffConfig::default();
        let tariff = tariff.unwrap_or(&unset);
        Tariff {
            rate: BytePrice::new(
                self.value(&format!("{key}.msat"), tariff.msat),
                self.value(&format!("{key}.per_bytes"), tariff.per_bytes),
            ),
            min_msat: self.value(&format!("{key}.min_msat"), tariff.min_msat),
        }
    }

    fn mints(&mut self, mints: &[String]) -> Vec<String> {
        let mut seen = HashSet::new();
        let mints: Vec<String> = mints
            .iter()
            .map(|mint| mint.trim().to_string())
            .filter(|mint| !mint.is_empty() && seen.insert(mint.clone()))
            .collect();
        if mints.is_empty() {
            self.missing.push("mints".to_string());
        }
        mints
    }

    fn finish(self) -> Result<()> {
        if self.missing.is_empty() {
            return Ok(());
        }
        let section = self.section;
        if self.missing == ["mints"] {
            bail!("{section}.mints lists no mint; none is chosen by default");
        }
        bail!(
            "{section} does not set {}; no price, mint or cap is chosen by default",
            self.missing.join(", ")
        );
    }
}

fn provider_settings(provider: &PaidProviderConfig, state_dir: &Path) -> Result<ProviderSettings> {
    let section = "paid_retrieval.provider";
    let mut required = Required::new(section);
    let tariff = required.tariff("tariff", provider.tariff.as_ref());
    let mints = required.mints(&provider.mints);
    required.finish()?;
    if tariff.rate.per_bytes == 0 {
        bail!("{section}.tariff.per_bytes must be at least 1");
    }
    if provider.min_topup_sat == 0 {
        bail!("{section}.min_topup_sat must be at least 1");
    }
    let ledger_dir = match provider.ledger_dir.as_deref().map(str::trim) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => state_dir.join("ledger"),
    };
    let mut config = ProviderConfig::new(tariff, mints);
    config.min_topup_sat = provider.min_topup_sat;
    config.grace_msat = provider.grace_msat;
    config.lock = match provider.lock.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(key) => Some(lock_key(key).ok_or_else(|| {
            anyhow::anyhow!("{section}.lock must be a compressed public key: 66 hex digits")
        })?),
    };
    Ok(ProviderSettings {
        config,
        ledger_dir,
        paying_only: provider.paying_only,
        http: provider.http,
    })
}

/// A compressed secp256k1 public key in hex.
fn lock_key(hex_key: &str) -> Option<LockKey> {
    let key: LockKey = hex::decode(hex_key).ok()?.try_into().ok()?;
    matches!(key[0], 0x02 | 0x03).then_some(key)
}

fn client_settings(client: &PaidClientConfig, state_dir: PathBuf) -> Result<ClientSettings> {
    let section = "paid_retrieval.client";
    let mut required = Required::new(section);
    let spending_limit_sat = required.value("spending_limit_sat", client.spending_limit_sat);
    let max_tariff = required.tariff("max_tariff", client.max_tariff.as_ref());
    let mints = required.mints(&client.mints);
    let policy = ClientPolicy {
        max_tariff,
        mints,
        topup_sat: required.value("topup_sat", client.topup_sat),
        max_prepaid_msat: required.value("max_prepaid_msat", client.max_prepaid_msat),
        max_loss_msat: required.value("max_loss_msat", client.max_loss_msat),
        max_token_fee_sat: required.value("max_token_fee_sat", client.max_token_fee_sat),
        max_redeem_fee_msat: required.value("max_redeem_fee_msat", client.max_redeem_fee_msat),
        // A time, not money: the library's deadline for one read or payment.
        request_timeout: ClientPolicy::default().request_timeout,
    };
    required.finish()?;
    if policy.max_tariff.rate.per_bytes == 0 {
        bail!("{section}.max_tariff.per_bytes must be at least 1");
    }
    if policy.topup_sat == 0 {
        bail!("{section}.topup_sat must be at least 1");
    }
    if policy.topup_sat.saturating_mul(1_000) > policy.max_prepaid_msat {
        bail!("{section}.topup_sat is more than max_prepaid_msat allows a provider to hold");
    }
    let mut names = HashSet::new();
    let mut places = HashSet::new();
    let mut providers = Vec::with_capacity(client.providers.len());
    for provider in &client.providers {
        let name = provider.name.trim().to_string();
        if name.is_empty() {
            bail!("{section}.providers has an entry without a name");
        }
        let npub = provider.npub.as_deref().map(str::trim);
        let url = provider.url.as_deref().map(str::trim);
        let place = match (npub, url) {
            (Some(npub), None) => {
                let Ok(identity) = PeerIdentity::from_npub(npub) else {
                    bail!("{section}.providers: {name} has no valid npub");
                };
                let npub = npub.to_string();
                ProviderPlace::Fips { npub, identity }
            }
            (None, Some(url)) => match super::http::paid_blob_endpoint(url) {
                Ok(url) => ProviderPlace::Http { url },
                Err(error) => bail!("{section}.providers: {name}: {error}"),
            },
            _ => bail!("{section}.providers: {name} needs either npub or url"),
        };
        if !names.insert(name.clone()) || !places.insert(place.label()) {
            bail!("{section}.providers lists {name} or its address twice");
        }
        providers.push(ProviderPeer { name, place });
    }
    Ok(ClientSettings {
        policy,
        spending_limit_sat,
        pay_for_remote_reads: client.pay_for_remote_reads,
        providers,
        state_dir,
    })
}
