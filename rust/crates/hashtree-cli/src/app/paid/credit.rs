use super::*;
use hashtree_cli::paid_retrieval::credit::{bind_exchange, detect_receive_wallet, signing_key};
use hashtree_fips_transport::PeerIdentity;
use hashtree_paid_retrieval::credit::{CreditAccount, CreditBlobRoute};
use hashtree_paid_retrieval::{FipsPaidBlobService, FipsPaidConfig};
use std::io::{self, IsTerminal, Write};
use std::path::Path;

pub(super) async fn run(command: CreditCommands, config: &Config, data_dir: &Path) -> Result<()> {
    if let CreditCommands::WalletStatus { receive_rpc } = command {
        let configured = config
            .paid_retrieval
            .credit
            .as_ref()
            .and_then(|credit| credit.receive_rpc.clone());
        let wallet = detect_receive_wallet(receive_rpc.or(configured)).await?;
        println!("Core Lightning wallet connected on {}.", wallet.network);
        return Ok(());
    }
    let cfg = config
        .paid_retrieval
        .credit
        .as_ref()
        .context("Configure [paid_retrieval.credit] first")?;
    let keys = hashtree_cli::config::read_keys()?;
    if let CreditCommands::Init {
        trust,
        per_peer_sat,
        total_sat,
        lifetime_spend_sat,
        trial_peer_sat,
        trial_pool_sat,
    } = command
    {
        cfg.initialize(
            data_dir,
            &keys,
            &trust,
            per_peer_sat,
            total_sat,
            lifetime_spend_sat,
            trial_peer_sat
                .zip(trial_pool_sat)
                .map(|(initial_peer_sat, pool_sat)| peer_payments::TrialPolicy {
                    initial_peer_sat,
                    pool_sat,
                }),
        )?;
        println!("Peer credit account created.");
        return Ok(());
    }
    if let CreditCommands::Balance = command {
        let b = cfg.balance(data_dir, &keys)?;
        println!("Available credit {} sat\nOwed {} sat\nForwarding fees earned {} sat\nPayout fees earned {} sat\nLightning network fees paid {} msat",
            b.available_claims_sat,b.available_debts_sat,b.earnings.forwarding_fee_sat,b.earnings.terminal_fee_sat,b.earnings.network_fee_msat);
        return Ok(());
    }
    // Take exclusive ownership before opening the same network identity.
    let journal = cfg.open(data_dir, &keys)?;
    let store = Arc::new(HashtreeStore::new(data_dir)?);
    let fips = start_daemon_fips_transport(config, &keys, store.clone(), Vec::new())
        .await?
        .context("Enable FIPS for peer credit")?;
    let outcome: Result<()> = async {
        let link = bind_exchange(cfg, &fips, &keys, journal.clone()).await?;
        // A short-lived operator endpoint first has to reestablish its links.
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if fips
                    .endpoint
                    .peers()
                    .await?
                    .iter()
                    .any(|peer| peer.connected)
                {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await
        .context("No connected peer within 30 seconds")??;
        match command {
            CreditCommands::Quote { ref invoice } | CreditCommands::Pay { ref invoice, .. } => {
                let invoice = read_invoice(invoice)?;
                let quote = link
                    .quote_invoice(&invoice)
                    .await
                    .map_err(anyhow::Error::msg)?;
                println!("{}", quote_summary(&quote)?);
                if let CreditCommands::Pay {
                    ref receive_rpc, ..
                } = command
                {
                    let confirmed = tokio::task::spawn_blocking(|| -> io::Result<bool> {
                        if !io::stdin().is_terminal() {
                            return Err(io::Error::other(
                                "A terminal is required to confirm payment",
                            ));
                        }
                        eprint!("Pay this invoice using peer credit? [y/N] ");
                        io::stderr().flush()?;
                        let mut answer = String::new();
                        io::stdin().read_line(&mut answer)?;
                        Ok(answer.trim().eq_ignore_ascii_case("y")
                            || answer.trim().eq_ignore_ascii_case("yes"))
                    })
                    .await??;
                    if confirmed {
                        let state = match receive_rpc {
                            Some(rpc) => link.confirm_cashout(&invoice, &quote, rpc).await,
                            None => link.confirm_invoice(&invoice, &quote).await,
                        }
                        .map_err(anyhow::Error::msg)?;
                        report_payment(&link, &quote.payment_hash, state, receive_rpc.as_deref())
                            .await?;
                    } else {
                        println!("Cancelled.");
                    }
                }
            }
            CreditCommands::OfferPayout { peer } => {
                let peer = PeerIdentity::from_npub(&peer)?;
                link.offer_payout(&peer.pubkey().to_string())
                    .await
                    .map_err(anyhow::Error::msg)?;
                println!("Private payout availability sent.");
            }
            CreditCommands::Resume {
                payment_hash,
                receive_rpc,
            } => {
                let state = match receive_rpc {
                    Some(rpc) => link.resume_cashout(&payment_hash, &rpc).await,
                    None => link.resume_invoice(&payment_hash).await,
                }
                .map_err(anyhow::Error::msg)?;
                println!("{}", serde_json::to_string(&state)?);
            }
            CreditCommands::Reconcile { provider, hash } => {
                let hash: [u8; 32] = hex::decode(hash)?
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("Expected a SHA-256 hash"))?;
                let peer = PeerIdentity::from_npub(&provider)?;
                let service = FipsPaidBlobService::credit(
                    fips.endpoint.clone(),
                    None,
                    FipsPaidConfig::default(),
                )
                .await?;
                let route = CreditBlobRoute::new(
                    CreditAccount::new(journal, signing_key(&keys)?)?,
                    peer.pubkey().to_string(),
                    Arc::new(service.transport_to(peer)),
                    1,
                    store.store_arc(),
                )?;
                anyhow::ensure!(
                    route.reconcile(hash).await?,
                    "No verified delivery receipt for this provider and hash"
                );
                println!("Receipt acknowledged.");
            }
            CreditCommands::Init { .. }
            | CreditCommands::WalletStatus { .. }
            | CreditCommands::Balance => unreachable!(),
        }
        Ok(())
    }
    .await;
    fips.shutdown().await;
    outcome
}

fn read_invoice(input: &str) -> Result<String> {
    use std::io::Read;
    let text = if let Some(path) = input.strip_prefix('@') {
        let mut text = String::new();
        std::fs::File::open(path)?
            .take(8193)
            .read_to_string(&mut text)?;
        text
    } else {
        input.to_owned()
    };
    anyhow::ensure!(text.len() <= 8192, "Invoice is too long");
    Ok(text.trim().to_owned())
}

fn quote_summary(quote: &fips_credit_link::InvoiceQuote) -> Result<String> {
    let debit = quote.debit_sat().map_err(anyhow::Error::msg)?;
    Ok(format!("Invoice {} sat\nConversion fee {} sat\nTotal peer credit {} sat\nDebtor {}\nPayment hash {}\nQuote expires at {}",quote.amount_sat,quote.fee_sat,debit,quote.debtor,quote.payment_hash,quote.expires_at))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn confirmation_shows_the_full_debit_before_spending() {
        let quote: fips_credit_link::InvoiceQuote=serde_json::from_value(serde_json::json!({
            "agreement_id":"example","debtor":"example","payment_hash":"example","amount_sat":7,"fee_sat":2,"expires_at":30
        })).unwrap();
        let summary = quote_summary(&quote).unwrap();
        assert!(
            summary.starts_with("Invoice 7 sat\nConversion fee 2 sat\nTotal peer credit 9 sat\n")
        );
    }
}

async fn report_payment(
    link: &fips_credit_link::CreditLink,
    hash: &str,
    mut state: fips_credit_link::PaymentState,
    receive_rpc: Option<&Path>,
) -> Result<()> {
    if state == fips_credit_link::PaymentState::Pending {
        eprintln!("Payment pending. Checking the same invoice.");
        let _ = tokio::time::timeout(Duration::from_secs(15), async {
            for delay in [1, 2, 4] {
                tokio::time::sleep(Duration::from_secs(delay)).await;
                let check = match receive_rpc {
                    Some(rpc) => link.resume_cashout(hash, rpc).await,
                    None => link.resume_invoice(hash).await,
                };
                if let Ok(updated) = check {
                    state = updated;
                }
                if state == fips_credit_link::PaymentState::Paid {
                    break;
                }
            }
        })
        .await;
    }
    println!("{}", serde_json::to_string(&state)?);
    if state == fips_credit_link::PaymentState::Pending {
        eprintln!(
            "Resume the existing payment with htree paid credit resume {hash}{}",
            if receive_rpc.is_some() {
                " --receive-rpc <wallet-socket>"
            } else {
                ""
            }
        );
    }
    Ok(())
}
