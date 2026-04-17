use anyhow::Result;
use clap::{Parser, Subcommand};
use hashtree_cli::cashu::{
    load_mint_balance, load_wallet_activity, load_wallet_overview, receive_payment_token,
    revoke_pending_payment, send_lightning_payment, send_payment_token,
};
use hashtree_cli::cashu_cli::{
    add_mint, list_mints, print_balance, remove_mint, resolve_selected_mint, set_default_mint,
    topup_balance,
};
use hashtree_cli::config::get_hashtree_dir;
use hashtree_cli::Config;
use serde_json::json;
use std::path::PathBuf;
use tokio::io::AsyncReadExt;

#[derive(Parser)]
#[command(name = "htree-cashu")]
#[command(version)]
#[command(about = "Cashu wallet helper for hashtree", long_about = None)]
struct Cli {
    /// Data directory (default: ~/.hashtree/data)
    #[arg(long, global = true, env = "HTREE_DATA_DIR")]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

impl Cli {
    fn data_dir(&self) -> PathBuf {
        self.data_dir
            .clone()
            .unwrap_or_else(|| get_hashtree_dir().join("data"))
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Show Cashu wallet balances
    #[command(visible_alias = "status")]
    Balance {
        /// Show only one mint
        #[arg(long)]
        mint: Option<String>,
    },
    /// Create a Cashu top-up quote from the selected mint
    #[command(visible_alias = "load")]
    Topup {
        /// Amount in satoshis
        amount_sat: u64,
        /// Mint to use (defaults to configured default mint)
        #[arg(long)]
        mint: Option<String>,
    },
    /// Pay a BOLT11 invoice from the selected mint
    SendPayment {
        /// BOLT11 payment request
        payment_request: String,
        /// Mint to use (defaults to configured default mint)
        #[arg(long)]
        mint: Option<String>,
    },
    /// Manage accepted Cashu mints
    Mint {
        #[command(subcommand)]
        command: MintCommands,
    },
    #[command(hide = true)]
    Internal {
        #[command(subcommand)]
        command: InternalCommands,
    },
}

#[derive(Subcommand)]
enum MintCommands {
    /// List accepted mints
    List,
    /// Add an accepted mint
    Add {
        /// Mint base URL
        url: String,
        /// Also set as default mint
        #[arg(long = "default")]
        make_default: bool,
    },
    /// Remove an accepted mint
    Remove {
        /// Mint base URL
        url: String,
    },
    /// Set the default mint
    Default {
        /// Mint base URL
        url: String,
    },
}

#[derive(Subcommand)]
enum InternalCommands {
    Overview,
    Mints,
    Balance {
        #[arg(long)]
        mint: String,
    },
    Send {
        amount_sat: u64,
        #[arg(long)]
        mint: String,
    },
    Receive {
        #[arg(long)]
        token: Option<String>,
        #[arg(long)]
        token_stdin: bool,
    },
    Revoke {
        #[arg(long)]
        mint: String,
        #[arg(long)]
        operation_id: String,
    },
    MintAdd {
        url: String,
        #[arg(long = "default")]
        make_default: bool,
    },
    MintRemove {
        url: String,
    },
    MintDefault {
        url: String,
    },
}

fn mint_state_json(config: &Config) -> serde_json::Value {
    json!({
        "accepted_mints": config.cashu.accepted_mints.clone(),
        "default_mint": config.cashu.default_mint.clone(),
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();
    let data_dir = cli.data_dir();
    let mut config = Config::load()?;

    match cli.command {
        Commands::Balance { mint } => {
            print_balance(&config, &data_dir, mint.as_deref()).await?;
        }
        Commands::Topup { amount_sat, mint } => {
            topup_balance(&config, &data_dir, amount_sat, mint.as_deref()).await?;
        }
        Commands::SendPayment {
            payment_request,
            mint,
        } => {
            let mint_url = resolve_selected_mint(&config, mint.as_deref())?;
            let payment = send_lightning_payment(&data_dir, &mint_url, &payment_request).await?;
            println!("{}", serde_json::to_string(&payment)?);
        }
        Commands::Mint { command } => match command {
            MintCommands::List => {
                list_mints(&config);
            }
            MintCommands::Add { url, make_default } => {
                add_mint(&mut config, &url, make_default)?;
            }
            MintCommands::Remove { url } => {
                remove_mint(&mut config, &url)?;
            }
            MintCommands::Default { url } => {
                set_default_mint(&mut config, &url)?;
            }
        },
        Commands::Internal { command } => match command {
            InternalCommands::Overview => {
                let overview = load_wallet_overview(&data_dir, true).await?;
                let history = load_wallet_activity(&data_dir).await?;
                println!(
                    "{}",
                    json!({
                        "totals": overview.totals.iter().map(|total| json!({
                            "unit": total.unit,
                            "balance": total.balance,
                        })).collect::<Vec<_>>(),
                        "entries": overview.entries.iter().map(|entry| json!({
                            "mint_url": entry.mint_url,
                            "unit": entry.unit,
                            "balance": entry.balance,
                        })).collect::<Vec<_>>(),
                        "warnings": overview.warnings,
                        "legacy_state_detected": overview.legacy_state_detected,
                        "accepted_mints": config.cashu.accepted_mints.clone(),
                        "default_mint": config.cashu.default_mint.clone(),
                        "history": history.iter().map(|entry| json!({
                            "id": entry.id,
                            "kind": entry.kind,
                            "status": entry.status,
                            "mint_url": entry.mint_url,
                            "unit": entry.unit,
                            "amount_sat": entry.amount_sat,
                            "fee_sat": entry.fee_sat,
                            "created_at_unix": entry.created_at_unix,
                            "expires_at_unix": entry.expires_at_unix,
                            "quote_id": entry.quote_id,
                            "operation_id": entry.operation_id,
                            "payment_request": entry.payment_request,
                            "token": entry.token,
                        })).collect::<Vec<_>>(),
                    })
                );
            }
            InternalCommands::Mints => {
                println!("{}", mint_state_json(&config));
            }
            InternalCommands::Balance { mint } => {
                let balance = load_mint_balance(&data_dir, &mint).await?;
                println!("{}", serde_json::to_string(&balance)?);
            }
            InternalCommands::Send { amount_sat, mint } => {
                let payment = send_payment_token(&data_dir, &mint, amount_sat).await?;
                println!("{}", serde_json::to_string(&payment)?);
            }
            InternalCommands::Receive { token, token_stdin } => {
                let token = if token_stdin {
                    let mut buf = String::new();
                    tokio::io::stdin().read_to_string(&mut buf).await?;
                    buf.trim().to_string()
                } else {
                    token.ok_or_else(|| anyhow::anyhow!("missing --token or --token-stdin"))?
                };
                let payment = receive_payment_token(&data_dir, &token).await?;
                println!("{}", serde_json::to_string(&payment)?);
            }
            InternalCommands::Revoke { mint, operation_id } => {
                let revoked_sat = revoke_pending_payment(&data_dir, &mint, &operation_id).await?;
                println!("{}", json!({ "revoked_sat": revoked_sat }));
            }
            InternalCommands::MintAdd { url, make_default } => {
                add_mint(&mut config, &url, make_default)?;
                println!("{}", mint_state_json(&config));
            }
            InternalCommands::MintRemove { url } => {
                remove_mint(&mut config, &url)?;
                println!("{}", mint_state_json(&config));
            }
            InternalCommands::MintDefault { url } => {
                set_default_mint(&mut config, &url)?;
                println!("{}", mint_state_json(&config));
            }
        },
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_parses_topup_alias_and_mint_add() {
        let cli = Cli::parse_from([
            "htree-cashu",
            "load",
            "21",
            "--mint",
            "https://mint.example",
        ]);
        match cli.command {
            Commands::Topup { amount_sat, mint } => {
                assert_eq!(amount_sat, 21);
                assert_eq!(mint.as_deref(), Some("https://mint.example"));
            }
            _ => panic!("expected topup command"),
        }

        let cli = Cli::parse_from([
            "htree-cashu",
            "send-payment",
            "lnbc1example",
            "--mint",
            "https://mint.example",
        ]);
        match cli.command {
            Commands::SendPayment {
                payment_request,
                mint,
            } => {
                assert_eq!(payment_request, "lnbc1example");
                assert_eq!(mint.as_deref(), Some("https://mint.example"));
            }
            _ => panic!("expected send-payment command"),
        }

        let cli = Cli::parse_from([
            "htree-cashu",
            "mint",
            "add",
            "https://mint.example",
            "--default",
        ]);
        match cli.command {
            Commands::Mint {
                command: MintCommands::Add { url, make_default },
            } => {
                assert_eq!(url, "https://mint.example");
                assert!(make_default);
            }
            _ => panic!("expected mint add command"),
        }
    }

    #[test]
    fn test_cli_parses_internal_commands() {
        let cli = Cli::parse_from(["htree-cashu", "internal", "overview"]);
        match cli.command {
            Commands::Internal {
                command: InternalCommands::Overview,
            } => {}
            _ => panic!("expected internal overview command"),
        }

        let cli = Cli::parse_from([
            "htree-cashu",
            "internal",
            "balance",
            "--mint",
            "https://mint.example",
        ]);
        match cli.command {
            Commands::Internal {
                command: InternalCommands::Balance { mint },
            } => {
                assert_eq!(mint, "https://mint.example");
            }
            _ => panic!("expected internal balance command"),
        }

        let cli = Cli::parse_from([
            "htree-cashu",
            "internal",
            "send",
            "3",
            "--mint",
            "https://mint.example",
        ]);
        match cli.command {
            Commands::Internal {
                command: InternalCommands::Send { amount_sat, mint },
            } => {
                assert_eq!(amount_sat, 3);
                assert_eq!(mint, "https://mint.example");
            }
            _ => panic!("expected internal send command"),
        }

        let cli = Cli::parse_from(["htree-cashu", "internal", "receive", "--token-stdin"]);
        match cli.command {
            Commands::Internal {
                command: InternalCommands::Receive { token, token_stdin },
            } => {
                assert!(token.is_none());
                assert!(token_stdin);
            }
            _ => panic!("expected internal receive command"),
        }

        let cli = Cli::parse_from([
            "htree-cashu",
            "internal",
            "mint-add",
            "https://mint.example",
            "--default",
        ]);
        match cli.command {
            Commands::Internal {
                command: InternalCommands::MintAdd { url, make_default },
            } => {
                assert_eq!(url, "https://mint.example");
                assert!(make_default);
            }
            _ => panic!("expected internal mint add command"),
        }
    }
}
