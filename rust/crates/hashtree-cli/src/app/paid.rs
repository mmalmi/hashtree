//! `htree paid`: the client's paid-retrieval accounts, for an operator.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use hashtree_cli::cashu_helper::CashuHelperClient;
use hashtree_cli::config::{ensure_keys, Config};
use hashtree_cli::fips_transport::start_daemon_fips_transport;
use hashtree_cli::paid_retrieval::{
    clear_block, ensure_accounts_free, format_paid_status, offline_status, settle,
    PaidRetrievalSettings,
};
use hashtree_cli::HashtreeStore;
use hashtree_paid_retrieval::CashuWallet;

use super::args::PaidCommands;

pub(crate) async fn run(command: PaidCommands, cli_data_dir: Option<PathBuf>) -> Result<()> {
    let config = Config::load()?;
    // As `htree start` resolves it, so that both use the same accounts.
    let data_dir = cli_data_dir.unwrap_or_else(|| PathBuf::from(&config.storage.data_dir));
    match command {
        PaidCommands::Status { addr } => {
            let addr = addr.unwrap_or_else(|| config.server.bind_address.clone());
            print_status(&config, &data_dir, &addr).await
        }
        PaidCommands::ClearBlock { provider } => {
            let cleared = clear_block(&config, &data_dir, &provider)?;
            println!(
                "{provider} is paid again; {:.3} sat written off.\nIt was blocked because: {}",
                cleared.written_off_msat as f64 / 1_000.0,
                cleared.reason
            );
            Ok(())
        }
        PaidCommands::Settle => run_settle(&config, &data_dir).await,
    }
}

async fn print_status(config: &Config, data_dir: &std::path::Path, addr: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .context("Failed to build HTTP client")?;
    let url = format!("http://{addr}/api/status");
    let running = match client.get(&url).send().await {
        Ok(response) if response.status().is_success() => {
            Some(response.json::<serde_json::Value>().await?)
        }
        _ => None,
    };
    let (heading, paid) = match running {
        Some(status) => (
            format!("Paid retrieval (daemon at {addr}):"),
            status["paid_retrieval"].clone(),
        ),
        None => (
            "Paid retrieval (daemon not running; from the account files):".to_string(),
            offline_status(config, data_dir)?,
        ),
    };
    if paid.is_null() {
        println!("Paid retrieval is not configured ([paid_retrieval] in config.toml).");
        return Ok(());
    }
    println!("{heading}");
    for line in format_paid_status(&paid) {
        println!("{line}");
    }
    Ok(())
}

async fn run_settle(config: &Config, data_dir: &std::path::Path) -> Result<()> {
    // Before anything binds: the daemon may hold the accounts and its identity.
    ensure_accounts_free(config, data_dir)?;
    let wallet_mints = &config.cashu.accepted_mints;
    let settings =
        PaidRetrievalSettings::from_config(&config.paid_retrieval, wallet_mints, data_dir)?;
    let Some(client) = settings.and_then(|settings| settings.client) else {
        bail!("no provider is configured in [paid_retrieval.client]");
    };
    let helper = CashuHelperClient::discover(data_dir)
        .context("Paid retrieval needs the htree-cashu wallet helper")?;
    let wallet = Arc::new(CashuWallet::new(helper, client.policy.mints.clone()));
    let over_fips = config
        .paid_retrieval
        .client_provider_npubs()
        .next()
        .is_some();
    let fips = if over_fips {
        // The daemon's identity: over FIPS the account is that identity.
        let (keys, _) = ensure_keys()?;
        let store = Arc::new(HashtreeStore::new(data_dir)?);
        start_daemon_fips_transport(config, &keys, store, Vec::new()).await?
    } else {
        None
    };
    let settled = settle(config, data_dir, fips.as_ref(), wallet).await;
    if let Some(fips) = &fips {
        fips.shutdown().await;
    }
    let mut failed = 0;
    for (provider, result) in settled? {
        match result {
            Ok(()) => println!("{provider}: settled"),
            Err(error) => {
                failed += 1;
                println!("{provider}: {error}");
            }
        }
    }
    if failed > 0 {
        bail!("{failed} provider(s) could not be settled");
    }
    Ok(())
}
