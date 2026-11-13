//! `htree paid`: what an operator does with the client's accounts while the
//! daemon is not running. Each command takes the same lock as the daemon and
//! refuses while another process holds it.

use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use hashtree_core::StoreError;
use hashtree_paid_retrieval::{
    AccountStore, FileAccountStore, FipsPaidBlobService, FipsPaidConfig, PaidBlobRoute,
    PaidBlobTransport, PaymentRequest, RetrievalWallet, WalletError, WalletPayment,
};
use serde_json::{json, Value};

use super::{
    account_file, lock_accounts, open_client, ClientSettings, DaemonPaidRetrieval, FileSpendStore,
    PaidRetrievalSettings, ProviderPlace,
};
use crate::config::Config;
use crate::fips_transport::DaemonFipsHandle;

fn client_settings(config: &Config, data_dir: &Path) -> Result<ClientSettings> {
    let wallet_mints = &config.cashu.accepted_mints;
    let settings =
        PaidRetrievalSettings::from_config(&config.paid_retrieval, wallet_mints, data_dir)?;
    match settings.and_then(|settings| settings.client) {
        Some(client) => Ok(client),
        None => bail!("no provider is configured in [paid_retrieval.client]"),
    }
}

/// Fail when another process, normally the running daemon, holds the
/// client's accounts.
pub fn ensure_accounts_free(config: &Config, data_dir: &Path) -> Result<()> {
    let settings = client_settings(config, data_dir)?;
    drop(lock_accounts(&settings.state_dir)?);
    Ok(())
}

/// What `clear-block` wrote off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClearedBlock {
    pub reason: String,
    pub written_off_msat: u64,
}

/// A carrier for a route that is only edited, never asked.
struct NoCarrier;

#[async_trait]
impl PaidBlobTransport for NoCarrier {
    async fn exchange(&self, _request: Vec<u8>) -> Result<Vec<u8>, StoreError> {
        Err(StoreError::Other(
            "nothing is sent while editing an account".into(),
        ))
    }
}

/// A wallet for a route that is only edited, never asked to pay.
struct NoWallet;

#[async_trait]
impl RetrievalWallet for NoWallet {
    async fn pay(&self, _request: &PaymentRequest) -> Result<WalletPayment, WalletError> {
        Err(WalletError::Failed(
            "nothing is paid while editing an account".into(),
        ))
    }

    async fn balance_sat(&self) -> Result<u64, WalletError> {
        Ok(0)
    }
}

/// Pay `provider` again: write off the loss it was blocked for, as
/// `PaidBlobRoute::clear_block` does in the daemon, and save the account.
pub fn clear_block(config: &Config, data_dir: &Path, provider: &str) -> Result<ClearedBlock> {
    let settings = client_settings(config, data_dir)?;
    let Some(peer) = settings.providers.iter().find(|peer| peer.name == provider) else {
        bail!("no provider named {provider} is configured");
    };
    let _lock = lock_accounts(&settings.state_dir)?;
    let accounts = settings.state_dir.join("accounts");
    let store = FileAccountStore::new(accounts.join(account_file(&peer.place)));
    let Some(account) = store.load().context("Failed to read the paid account")? else {
        bail!("{provider} has no account yet");
    };
    let Some(reason) = account.blocked.clone() else {
        bail!("{provider} is not blocked");
    };
    let written_off_msat = account.loss_msat();
    let route = PaidBlobRoute::with_account(
        provider,
        Arc::new(NoCarrier),
        Arc::new(NoWallet),
        settings.policy,
        account,
    );
    route.clear_block();
    store
        .save(&route.account_state())
        .map_err(anyhow::Error::msg)
        .context("Failed to save the paid account")?;
    Ok(ClearedBlock {
        reason,
        written_off_msat,
    })
}

/// Resend unacknowledged payments and try unresolved tokens again, with the
/// daemon's wallet and carriers. `fips` is needed for providers named by npub.
pub async fn settle(
    config: &Config,
    data_dir: &Path,
    fips: Option<&DaemonFipsHandle>,
    wallet: Arc<dyn RetrievalWallet>,
) -> Result<Vec<(String, Result<(), StoreError>)>> {
    let settings = client_settings(config, data_dir)?;
    let over_fips = settings
        .providers
        .iter()
        .any(|peer| matches!(peer.place, ProviderPlace::Fips { .. }));
    // Refuse before binding anything when the daemon holds the accounts.
    drop(lock_accounts(&settings.state_dir)?);
    let service = match fips {
        Some(fips) if over_fips => Some(
            FipsPaidBlobService::client(fips.endpoint.clone(), FipsPaidConfig::default())
                .await
                .context("Failed to bind the paid blob service")?,
        ),
        _ => None,
    };
    let bind_address = &config.server.bind_address;
    // Settling fetches no blob, so nothing needs a cache.
    let buying = open_client(settings, bind_address, service.as_ref(), wallet, None)?;
    let paid = DaemonPaidRetrieval {
        service: tokio::sync::Mutex::new(service),
        selling: None,
        buying: Some(buying),
    };
    let settled = paid.settle().await;
    paid.shutdown().await;
    Ok(settled)
}

/// What the account files say, for `htree paid status` while the daemon is
/// not running. Counts since a start exist only in a running daemon.
pub fn offline_status(config: &Config, data_dir: &Path) -> Result<Value> {
    let wallet_mints = &config.cashu.accepted_mints;
    let settings =
        PaidRetrievalSettings::from_config(&config.paid_retrieval, wallet_mints, data_dir)?;
    let Some(settings) = settings else {
        return Ok(Value::Null);
    };
    let provider = settings.provider.map(|provider| {
        let tariff = provider.config.tariff;
        json!({
            "tariff": {
                "msat": tariff.rate.msat,
                "per_bytes": tariff.rate.per_bytes,
                "min_msat": tariff.min_msat,
            },
            "mints": provider.config.mints,
            "paying_only": provider.paying_only,
            "http": provider.http,
        })
    });
    let client = match settings.client {
        None => None,
        Some(client) => {
            let accounts = client.state_dir.join("accounts");
            let spent = FileSpendStore::new(client.state_dir.join("spent"));
            let spent_sat = spent.load().context("Failed to read the spent total")?;
            let mut providers = Vec::new();
            for peer in &client.providers {
                let store = FileAccountStore::new(accounts.join(account_file(&peer.place)));
                let account = store.load().context("Failed to read a paid account")?;
                let account = account.unwrap_or_default();
                providers.push(json!({
                    "name": peer.name,
                    "address": peer.place.label(),
                    "balance_msat": account.balance_msat,
                    "unresolved_tokens": account.unresolved.len(),
                    "unresolved_sat": account.unresolved_msat() / 1_000,
                    "payment_pending": account.pending.is_some() || account.intent.is_some(),
                    "blocked": account.blocked,
                }));
            }
            Some(json!({
                "spending_limit_sat": client.spending_limit_sat,
                "spent_sat": spent_sat,
                "remaining_sat": client.spending_limit_sat.saturating_sub(spent_sat),
                "providers": providers,
            }))
        }
    };
    Ok(json!({ "provider": provider, "client": client }))
}
