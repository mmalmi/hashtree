use crate::cashu::{
    create_topup_quote, legacy_cashu_wallet_state_path, load_wallet_overview, normalize_mint_url,
    CashuWalletEntry,
};
use crate::Config;
use anyhow::{bail, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub async fn print_balance(
    config: &Config,
    data_dir: &Path,
    mint_filter: Option<&str>,
) -> Result<()> {
    let overview = load_wallet_overview(data_dir, true).await?;
    let normalized_filter = mint_filter.map(normalize_mint_url).transpose()?;

    if let Some(ref mint_url) = normalized_filter {
        print_single_mint_balance(config, &overview.entries, mint_url);
        print_footer(data_dir, &overview.warnings);
        return Ok(());
    }

    println!("Cashu balance");
    if overview.totals.is_empty() {
        println!("Total: 0 sat");
    } else {
        println!("Totals:");
        for total in &overview.totals {
            println!("  - {} {}", total.balance, total.unit);
        }
    }
    if let Some(default_mint) = &config.cashu.default_mint {
        println!("Default mint: {default_mint}");
    } else {
        println!("Default mint: none");
    }

    let mut grouped: BTreeMap<String, Vec<&CashuWalletEntry>> = BTreeMap::new();
    for entry in &overview.entries {
        grouped
            .entry(entry.mint_url.clone())
            .or_default()
            .push(entry);
    }

    let mut mint_urls: BTreeSet<String> = config.cashu.accepted_mints.iter().cloned().collect();
    mint_urls.extend(grouped.keys().cloned());

    if mint_urls.is_empty() {
        println!("Accepted mints: none configured");
        println!("Use `htree cashu mint add <url>` to accept a mint.");
        print_footer(data_dir, &overview.warnings);
        return Ok(());
    }

    println!("Mints:");
    for mint_url in mint_urls {
        let accepted = config
            .cashu
            .accepted_mints
            .iter()
            .any(|mint| mint == &mint_url);
        let default = config.cashu.default_mint.as_deref() == Some(mint_url.as_str());
        let flags = flags_for_mint(accepted, default);
        if let Some(entries) = grouped.get(&mint_url) {
            for entry in entries {
                println!(
                    "  - {} ({}) :: {} [{}]",
                    mint_url,
                    entry.unit,
                    format_amount(entry.balance, &entry.unit),
                    flags.join(", ")
                );
            }
        } else {
            println!("  - {} (sat) :: 0 sat [{}]", mint_url, flags.join(", "));
        }
    }

    print_footer(data_dir, &overview.warnings);
    Ok(())
}

pub async fn topup_balance(
    config: &Config,
    data_dir: &Path,
    amount_sat: u64,
    mint: Option<&str>,
) -> Result<()> {
    let mint_url = resolve_selected_mint(config, mint)?;
    let quote = create_topup_quote(data_dir, &mint_url, amount_sat).await?;

    println!("Created Cashu mint quote");
    println!("Mint: {}", quote.mint_url);
    println!("Amount: {} {}", quote.amount, quote.unit);
    println!("Quote id: {}", quote.quote_id);
    if quote.expiry_unix > 0 {
        println!("Expiry (unix): {}", quote.expiry_unix);
    }
    println!("Invoice:");
    println!("{}", quote.payment_request);
    println!(
        "After paying, run `htree cashu balance --mint {}` to mint pending proofs.",
        quote.mint_url
    );

    if legacy_cashu_wallet_state_path(data_dir).exists() {
        println!(
            "Legacy note: local dev wallet state at {} is ignored by the CDK wallet backend.",
            legacy_cashu_wallet_state_path(data_dir).display()
        );
    }

    Ok(())
}

pub fn list_mints(config: &Config) {
    if config.cashu.accepted_mints.is_empty() {
        println!("No accepted Cashu mints configured.");
        return;
    }

    println!("Accepted Cashu mints:");
    for mint_url in &config.cashu.accepted_mints {
        let suffix = if config.cashu.default_mint.as_deref() == Some(mint_url.as_str()) {
            " (default)"
        } else {
            ""
        };
        println!("  - {mint_url}{suffix}");
    }
}

pub fn add_mint(config: &mut Config, raw_url: &str, make_default: bool) -> Result<()> {
    let mint_url = normalize_mint_url(raw_url)?;
    if !config
        .cashu
        .accepted_mints
        .iter()
        .any(|mint| mint == &mint_url)
    {
        config.cashu.accepted_mints.push(mint_url.clone());
        config.cashu.accepted_mints.sort();
    }

    if make_default || config.cashu.default_mint.is_none() {
        config.cashu.default_mint = Some(mint_url.clone());
    }

    config.save()?;

    println!("Accepted mint: {mint_url}");
    if config.cashu.default_mint.as_deref() == Some(mint_url.as_str()) {
        println!("Default mint: {mint_url}");
    }
    Ok(())
}

pub fn remove_mint(config: &mut Config, raw_url: &str) -> Result<()> {
    let mint_url = normalize_mint_url(raw_url)?;
    let original_len = config.cashu.accepted_mints.len();
    config.cashu.accepted_mints.retain(|mint| mint != &mint_url);

    if config.cashu.accepted_mints.len() == original_len {
        bail!("Mint not found in accepted list: {mint_url}");
    }

    if config.cashu.default_mint.as_deref() == Some(mint_url.as_str()) {
        config.cashu.default_mint = config.cashu.accepted_mints.first().cloned();
    }

    config.save()?;
    println!("Removed mint: {mint_url}");
    match &config.cashu.default_mint {
        Some(default_mint) => println!("Default mint: {default_mint}"),
        None => println!("Default mint: none"),
    }
    Ok(())
}

pub fn set_default_mint(config: &mut Config, raw_url: &str) -> Result<()> {
    let mint_url = normalize_mint_url(raw_url)?;
    if !config
        .cashu
        .accepted_mints
        .iter()
        .any(|mint| mint == &mint_url)
    {
        bail!("Mint is not in accepted list: {mint_url}");
    }
    config.cashu.default_mint = Some(mint_url.clone());
    config.save()?;
    println!("Default mint: {mint_url}");
    Ok(())
}

fn print_single_mint_balance(config: &Config, entries: &[CashuWalletEntry], mint_url: &str) {
    let accepted = config
        .cashu
        .accepted_mints
        .iter()
        .any(|mint| mint == mint_url);
    let default = config.cashu.default_mint.as_deref() == Some(mint_url);
    let mint_entries: Vec<&CashuWalletEntry> = entries
        .iter()
        .filter(|entry| entry.mint_url == mint_url)
        .collect();

    println!("Mint: {mint_url}");
    println!("Accepted: {}", if accepted { "yes" } else { "no" });
    println!("Default: {}", if default { "yes" } else { "no" });
    if mint_entries.is_empty() {
        println!("Balance: 0 sat");
        return;
    }

    println!("Balances:");
    for entry in mint_entries {
        println!("  - {}", format_amount(entry.balance, &entry.unit));
    }
}

fn print_footer(data_dir: &Path, warnings: &[String]) {
    if legacy_cashu_wallet_state_path(data_dir).exists() {
        println!(
            "Legacy note: local dev wallet state at {} is ignored by the CDK wallet backend.",
            legacy_cashu_wallet_state_path(data_dir).display()
        );
    }
    if !warnings.is_empty() {
        println!("Warnings:");
        for warning in warnings {
            println!("  - {warning}");
        }
    }
}

fn flags_for_mint(accepted: bool, default: bool) -> Vec<&'static str> {
    let mut flags = Vec::new();
    if accepted {
        flags.push("accepted");
    } else {
        flags.push("stored-only");
    }
    if default {
        flags.push("default");
    }
    flags
}

fn format_amount(amount: u64, unit: &str) -> String {
    format!("{amount} {unit}")
}

pub fn resolve_selected_mint(config: &Config, mint: Option<&str>) -> Result<String> {
    if let Some(raw_mint) = mint {
        let mint_url = normalize_mint_url(raw_mint)?;
        if !config
            .cashu
            .accepted_mints
            .iter()
            .any(|accepted| accepted == &mint_url)
        {
            bail!("Mint is not accepted: {mint_url}");
        }
        return Ok(mint_url);
    }

    if let Some(default_mint) = &config.cashu.default_mint {
        return Ok(default_mint.clone());
    }

    bail!("No default Cashu mint configured. Use `htree cashu mint add <url> --default`.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_remove_and_default_mint() {
        let mut config = Config::default();
        add_mint_without_save(&mut config, "https://mint-b.example/", false).unwrap();
        add_mint_without_save(&mut config, "https://mint-a.example", true).unwrap();
        assert_eq!(
            config.cashu.accepted_mints,
            vec![
                "https://mint-a.example".to_string(),
                "https://mint-b.example".to_string()
            ]
        );
        assert_eq!(
            config.cashu.default_mint,
            Some("https://mint-a.example".to_string())
        );

        remove_mint_without_save(&mut config, "https://mint-a.example").unwrap();
        assert_eq!(
            config.cashu.default_mint,
            Some("https://mint-b.example".to_string())
        );
    }

    #[test]
    fn test_resolve_selected_mint_prefers_explicit_or_default() {
        let mut config = Config::default();
        config.cashu.accepted_mints = vec![
            "https://mint-a.example".to_string(),
            "https://mint-b.example".to_string(),
        ];
        config.cashu.default_mint = Some("https://mint-a.example".to_string());

        assert_eq!(
            resolve_selected_mint(&config, Some("https://mint-b.example/")).unwrap(),
            "https://mint-b.example"
        );
        assert_eq!(
            resolve_selected_mint(&config, None).unwrap(),
            "https://mint-a.example"
        );
        assert!(resolve_selected_mint(&config, Some("https://mint-c.example")).is_err());
    }

    fn add_mint_without_save(config: &mut Config, raw_url: &str, make_default: bool) -> Result<()> {
        let mint_url = normalize_mint_url(raw_url)?;
        if !config
            .cashu
            .accepted_mints
            .iter()
            .any(|mint| mint == &mint_url)
        {
            config.cashu.accepted_mints.push(mint_url.clone());
            config.cashu.accepted_mints.sort();
        }
        if make_default || config.cashu.default_mint.is_none() {
            config.cashu.default_mint = Some(mint_url);
        }
        Ok(())
    }

    fn remove_mint_without_save(config: &mut Config, raw_url: &str) -> Result<()> {
        let mint_url = normalize_mint_url(raw_url)?;
        config.cashu.accepted_mints.retain(|mint| mint != &mint_url);
        if config.cashu.default_mint.as_deref() == Some(mint_url.as_str()) {
            config.cashu.default_mint = config.cashu.accepted_mints.first().cloned();
        }
        Ok(())
    }
}
