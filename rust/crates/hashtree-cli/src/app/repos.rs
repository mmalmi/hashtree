use anyhow::{Context, Result};
use git_remote_htree::nostr_client::{resolve_identity, NostrClient};
use nostr::{PublicKey, ToBech32};

pub(crate) async fn list_repos(owner: Option<&str>) -> Result<()> {
    let owner = owner
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("self");

    let (pubkey, _secret_key) = resolve_identity(owner)?;
    let config = hashtree_config::Config::load_or_default();
    let author = PublicKey::from_hex(&pubkey).context("Invalid owner pubkey")?;
    let owner_display = author.to_bech32().unwrap_or_else(|_| pubkey.clone());

    let client = NostrClient::new(&pubkey, None, None, false, &config)
        .context("Failed to initialize Nostr client")?;
    let repos = client.list_repos_async().await?;
    if repos.is_empty() {
        println!("No git repos found for {}.", owner_display);
        return Ok(());
    }

    println!("Git repos for {}:", owner_display);
    for repo_name in repos {
        println!("  htree://{}/{}", owner_display, repo_name);
    }

    Ok(())
}
