use anyhow::{ensure, Context, Result};
use git_remote_htree::discovery::{choose, explicit_target, search, suggestions};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

pub(crate) async fn target(
    input: &str,
    git: bool,
    non_interactive: bool,
    data_dir: &Path,
) -> Result<String> {
    if let Some(target) = explicit_target(input)? {
        return Ok(target);
    }
    eprintln!("Looking for matches in your social graph...");
    let results = search(input, git, Duration::from_secs(5), Some(data_dir)).await?;
    choose(
        &results,
        if git { "git clone" } else { "htree get" },
        non_interactive,
    )
}

pub(crate) async fn list(input: &str, git: bool, seconds: u64, data_dir: &Path) -> Result<()> {
    let results = search(input, git, Duration::from_secs(seconds), Some(data_dir)).await?;
    print!(
        "{}",
        suggestions(&results, if git { "git clone" } else { "htree get" })
    );
    Ok(())
}

pub(crate) async fn clone(
    input: &str,
    directory: Option<PathBuf>,
    non_interactive: bool,
    data_dir: &Path,
) -> Result<()> {
    let url = target(input, true, non_interactive, data_dir).await?;
    let url = if url.starts_with("htree://") {
        url
    } else {
        format!("htree://{url}")
    };
    let mut command = tokio::process::Command::new("git");
    command.args(["clone", "--", &url]);
    if let Some(directory) = directory {
        command.arg(directory);
    }
    let status = command.status().await.context("Could not run git clone")?;
    ensure!(status.success(), "git clone failed with {status}");
    Ok(())
}
