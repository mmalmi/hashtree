use anyhow::{Context, Result};
use serde_json::to_string;
use std::path::PathBuf;

use hashtree_cli::pwa::install_site_pwa_to_store;
use hashtree_cli::HashtreeStore;

pub(crate) async fn run_export(data_dir: PathBuf, url: String, json: bool) -> Result<()> {
    let store = HashtreeStore::new(&data_dir).context("open hashtree store")?;
    let installed = install_site_pwa_to_store(&store, &url)
        .await
        .context("export installable PWA into hashtree")?;

    if json {
        println!(
            "{}",
            to_string(&installed).context("serialize PWA export result")?
        );
        return Ok(());
    }

    println!("saved {}", installed.name);
    println!("  launch:   {}", installed.launch_url);
    if let Some(icon_url) = installed.icon_url.as_deref() {
        println!("  icon:     {}", icon_url);
    }
    println!("  source:   {}", installed.source_url);
    println!("  manifest: {}", installed.source_manifest_url);
    if let Some(source_app_id) = installed.source_app_id.as_deref() {
        println!("  app id:   {}", source_app_id);
    }

    Ok(())
}
