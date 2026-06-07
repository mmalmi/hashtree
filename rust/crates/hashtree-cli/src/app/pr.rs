//! Pull request creation via NIP-34 (kind 1618)

use anyhow::{Context, Result};
#[cfg(test)]
use git_remote_htree::nostr_client::PullRequestStateFilter;
use git_remote_htree::nostr_client::{resolve_identity, NostrClient};
use nostr_sdk::prelude::*;
use std::process::Command;
use std::time::Duration;

use super::args::PrListState;

const KIND_PULL_REQUEST: u16 = 1618;
const KIND_REPO_ANNOUNCEMENT: u16 = 30617;

fn build_pr_view_url(target_npub: &str, repo_name: &str, nevent_id: &str) -> String {
    format!(
        "https://git.iris.to/#/{}/{}?tab=pulls&id={}",
        target_npub, repo_name, nevent_id
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RepoTargetSelection {
    InferFromGit,
    Explicit(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SourceBranchSelection {
    CurrentBranch,
    Explicit(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CloneUrlSelection {
    DefaultFromSelfAndRepo,
    Explicit(String),
}

struct CreatePrParamsInput<'a> {
    repo: Option<&'a str>,
    title: &'a str,
    description: Option<&'a str>,
    branch: Option<&'a str>,
    target_branch: &'a str,
    clone_url: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedCreatePrParams {
    repo: RepoTargetSelection,
    title: String,
    description: String,
    description_was_provided: bool,
    branch: SourceBranchSelection,
    target_branch: String,
    clone_url: CloneUrlSelection,
}

/// Create a pull request by publishing a kind 1618 event
pub(crate) async fn create_pr(
    repo: Option<&str>,
    title: &str,
    description: Option<&str>,
    branch: Option<&str>,
    target_branch: &str,
    clone_url: Option<&str>,
) -> Result<()> {
    ensure_git_work_tree()?;
    let params = normalize_create_pr_params(CreatePrParamsInput {
        repo,
        title,
        description,
        branch,
        target_branch,
        clone_url,
    })?;

    // 1. Resolve our own identity (contributor)
    let (_self_pubkey, self_secret) = resolve_identity("self")?;
    let self_secret: String =
        self_secret.context("No secret key found. Run 'htree user <nsec>' first.")?;

    let secret_bytes = hex::decode(&self_secret).context("Invalid secret key hex")?;
    let secret = nostr::SecretKey::from_slice(&secret_bytes)
        .map_err(|e| anyhow::anyhow!("Invalid secret key: {}", e))?;
    let keys = Keys::new(secret);
    let self_npub = keys
        .public_key()
        .to_bech32()
        .map_err(|e| anyhow::anyhow!("Failed to encode npub: {}", e))?;

    // 2. Resolve current branch + commit tip
    let source_branch = match &params.branch {
        SourceBranchSelection::CurrentBranch => git_current_branch()?,
        SourceBranchSelection::Explicit(branch) => branch.clone(),
    };
    let commit_tip = git_rev_parse(&source_branch)?;

    // 3. Resolve and parse target repo to get owner pubkey and repo name
    let repo_target = match &params.repo {
        RepoTargetSelection::InferFromGit => resolve_repo_target_input(None, &source_branch)?,
        RepoTargetSelection::Explicit(repo) => {
            resolve_repo_target_input(Some(repo), &source_branch)?
        }
    };
    let (target_pubkey, repo_name) = parse_repo_target(&repo_target)?;

    // 4. Build repo address tag: 30617:<target-owner-pubkey>:<repo-name>
    let repo_address = format!("{}:{}:{}", KIND_REPO_ANNOUNCEMENT, target_pubkey, repo_name);

    // 5. Build clone URL
    let clone_url = match &params.clone_url {
        CloneUrlSelection::Explicit(url) => url.clone(),
        CloneUrlSelection::DefaultFromSelfAndRepo => format!("htree://{}/{}", self_npub, repo_name),
    };

    // Best-effort safeguard: warn if the source branch is not pushed (no upstream) or upstream
    // doesn't include the commit we're about to reference in the PR event.
    warn_if_branch_not_pushed(&source_branch, &commit_tip);

    // 6. Build and publish kind 1618 event
    let mut tags = vec![
        Tag::custom(TagKind::custom("a"), vec![repo_address]),
        Tag::custom(TagKind::custom("p"), vec![target_pubkey.clone()]),
        Tag::custom(TagKind::custom("subject"), vec![params.title.clone()]),
        Tag::custom(TagKind::custom("branch"), vec![source_branch.clone()]),
        Tag::custom(
            TagKind::custom("target-branch"),
            vec![params.target_branch.clone()],
        ),
        Tag::custom(TagKind::custom("c"), vec![commit_tip.clone()]),
        Tag::custom(TagKind::custom("clone"), vec![clone_url]),
    ];

    // Add description tag if provided
    if params.description_was_provided {
        tags.push(Tag::custom(
            TagKind::custom("description"),
            vec![params.description.clone()],
        ));
    }

    let content = params.description.clone();
    let event = EventBuilder::new(Kind::Custom(KIND_PULL_REQUEST), &content)
        .tags(tags)
        .sign_with_keys(&keys)
        .map_err(|e| anyhow::anyhow!("Failed to sign event: {}", e))?;

    let event_id = event.id.to_hex();

    // Connect to relays and publish
    let config = hashtree_config::Config::load_or_default();
    let relays = hashtree_config::resolve_relays(&config.nostr.relays, None);

    let client = Client::new(keys);
    for relay in &relays {
        if let Err(e) = client.add_relay(relay).await {
            tracing::warn!("Failed to add relay {}: {}", relay, e);
        }
    }
    client.connect().await;

    // Wait for at least one relay
    let start = std::time::Instant::now();
    loop {
        let relay_map = client.relays().await;
        let mut connected = false;
        for relay in relay_map.values() {
            if relay.is_connected() {
                connected = true;
                break;
            }
        }
        if connected {
            break;
        }
        if start.elapsed() > Duration::from_secs(3) {
            anyhow::bail!("Failed to connect to any relay");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let publish_result = match client.send_event(&event).await {
        Ok(output) => {
            if output.success.is_empty() {
                Err(anyhow::anyhow!("PR event was not confirmed by any relay"))
            } else {
                Ok(())
            }
        }
        Err(e) => Err(anyhow::anyhow!("Failed to publish PR event: {}", e)),
    };

    let _ = client.disconnect().await;
    publish_result?;

    // Encode as nevent for display
    let event_id_obj =
        EventId::from_hex(&event_id).map_err(|e| anyhow::anyhow!("Invalid event id: {}", e))?;
    let relay_urls: Vec<RelayUrl> = relays
        .iter()
        .filter_map(|relay| RelayUrl::parse(relay).ok())
        .collect();
    let nevent = Nip19Event::new(event_id_obj).relays(relay_urls);
    let nevent_str = nevent
        .to_bech32()
        .map_err(|e| anyhow::anyhow!("Failed to encode nevent: {}", e))?;

    // Print results
    println!("PR created: '{}'", params.title);
    println!("  Branch: {} -> {}", source_branch, params.target_branch);
    println!("  Commit: {}", &commit_tip[..12]);
    println!("  Event: {}", nevent_str);

    // Build viewer URL
    let target_npub = PublicKey::from_hex(&target_pubkey)
        .ok()
        .and_then(|pk| pk.to_bech32().ok())
        .unwrap_or_else(|| target_pubkey.clone());
    println!(
        "  View: {}",
        build_pr_view_url(&target_npub, &repo_name, &nevent_str)
    );

    Ok(())
}

/// List pull requests for a repository.
pub(crate) async fn list_prs(repo: Option<&str>, state: PrListState) -> Result<()> {
    let repo_target = resolve_list_repo_target_input(repo)?;
    let (target_pubkey, repo_name) = parse_repo_target(&repo_target)?;
    let state_filter = state.to_filter();

    let config = hashtree_config::Config::load_or_default();
    let client = NostrClient::new(&target_pubkey, None, None, false, &config)
        .context("Failed to initialize Nostr client")?;

    let prs = client.fetch_prs_async(&repo_name, state_filter).await?;
    if prs.is_empty() {
        println!("No pull requests found.");
        return Ok(());
    }

    let target_display = PublicKey::from_hex(&target_pubkey)
        .ok()
        .and_then(|pk| pk.to_bech32().ok())
        .unwrap_or(target_pubkey.clone());
    println!(
        "Pull requests for {}/{} (state: {})",
        target_display,
        repo_name,
        state_filter.as_str()
    );

    for pr in prs {
        let subject = pr.subject.as_deref().unwrap_or("(no subject)");
        let event_short = pr.event_id.get(..12).unwrap_or(&pr.event_id);
        let branch = pr.branch.as_deref().unwrap_or("?");
        let target_branch = pr.target_branch.as_deref().unwrap_or("master");
        let commit = pr.commit_tip.as_deref().unwrap_or("?");
        let commit_short = commit.get(..12).unwrap_or(commit);

        println!("[{}] {} ({})", pr.state.as_str(), subject, event_short);
        println!("  Branch: {} -> {}", branch, target_branch);
        println!("  Commit: {}", commit_short);
    }

    Ok(())
}

fn resolve_list_repo_target_input(repo: Option<&str>) -> Result<String> {
    match repo.map(str::trim) {
        Some("") | None => infer_repo_target_from_git_for_list(),
        Some(raw) => resolve_repo_target_from_raw(raw, true),
    }
}

fn infer_repo_target_from_git_for_list() -> Result<String> {
    ensure_git_work_tree()?;

    if let Some(remote_url) = git_current_branch()
        .ok()
        .map(|source_branch| git_upstream_htree_remote_url_opt(&source_branch))
        .transpose()?
        .flatten()
    {
        return Ok(remote_url);
    }

    infer_repo_target_from_htree_remotes()
}

fn resolve_repo_target_from_raw(raw: &str, ensure_git_for_alias_errors: bool) -> Result<String> {
    if raw.starts_with("htree://") {
        return Ok(raw.to_string());
    }

    if let Some(remote_url) = git_remote_get_url_opt(raw)? {
        if !remote_url.starts_with("htree://") {
            anyhow::bail!(
                "Git remote '{}' is not an htree remote (URL: {}). Pass an htree remote alias or htree:// URL.",
                raw,
                remote_url
            );
        }
        return Ok(remote_url);
    }

    if raw.contains('/') {
        return Ok(raw.to_string());
    }

    if ensure_git_for_alias_errors {
        ensure_git_work_tree()?;
    }

    anyhow::bail!(
        "Invalid repo target '{}'. Expected a git remote alias, npub.../repo, or htree:// URL.",
        raw
    )
}

fn normalize_create_pr_params(input: CreatePrParamsInput<'_>) -> Result<NormalizedCreatePrParams> {
    let title = input.title.trim();
    if title.is_empty() {
        anyhow::bail!("PR title cannot be empty");
    }

    let normalized_repo = match input.repo.map(str::trim) {
        Some("") | None => RepoTargetSelection::InferFromGit,
        Some(repo) => RepoTargetSelection::Explicit(repo.to_string()),
    };

    let normalized_branch = match input.branch.map(str::trim) {
        Some("") | None => SourceBranchSelection::CurrentBranch,
        Some(branch) => SourceBranchSelection::Explicit(branch.to_string()),
    };

    let normalized_clone_url = match input.clone_url.map(str::trim) {
        Some("") | None => CloneUrlSelection::DefaultFromSelfAndRepo,
        Some(url) => CloneUrlSelection::Explicit(url.to_string()),
    };

    let normalized_target_branch = match input.target_branch.trim() {
        "" => "master".to_string(),
        branch => branch.to_string(),
    };

    let (description, description_was_provided) = match input.description {
        Some(description) => (description.to_string(), true),
        None => (String::new(), false),
    };

    Ok(NormalizedCreatePrParams {
        repo: normalized_repo,
        title: title.to_string(),
        description,
        description_was_provided,
        branch: normalized_branch,
        target_branch: normalized_target_branch,
        clone_url: normalized_clone_url,
    })
}

/// Parse repo target string into (pubkey_hex, repo_name)
/// Accepts: "npub1.../reponame", "htree://npub1.../reponame", "hex_pubkey/reponame"
fn parse_repo_target(repo: &str) -> Result<(String, String)> {
    let repo = sanitize_repo_target_path(repo);

    // Split on first /
    let (identity, repo_name) = repo
        .split_once('/')
        .context("Invalid repo format. Expected: npub.../reponame or htree://npub.../reponame")?;

    // Resolve the identity to a pubkey hex
    let (pubkey, _) = resolve_identity(identity)?;

    Ok((pubkey, repo_name.to_string()))
}

fn sanitize_repo_target_path(repo: &str) -> &str {
    // htree URLs may include fragments (#k=..., #private). Those are not part of the
    // repo name/address and must not leak into PR metadata tags.
    let repo = repo.strip_prefix("htree://").unwrap_or(repo);
    repo.split('#').next().unwrap_or(repo).trim_end_matches('/')
}

fn resolve_repo_target_input(repo: Option<&str>, source_branch: &str) -> Result<String> {
    let raw = match repo {
        Some(repo) => repo.to_string(),
        None => infer_repo_target_from_git(source_branch)?,
    };

    resolve_repo_target_from_raw(&raw, false)
}

fn infer_repo_target_from_git(source_branch: &str) -> Result<String> {
    if let Some(remote_url) = git_upstream_htree_remote_url_opt(source_branch)? {
        return Ok(remote_url);
    }

    infer_repo_target_from_htree_remotes()
}

fn infer_repo_target_from_htree_remotes() -> Result<String> {
    let mut htree_remotes = git_htree_remotes()?;

    match htree_remotes.len() {
        1 => Ok(htree_remotes.remove(0).1),
        0 => anyhow::bail!(
            "Could not infer target repo: no htree git remotes found. Pass a remote alias or htree:// URL."
        ),
        _ => {
            let names = htree_remotes
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "Could not infer target repo: multiple htree git remotes found ({names}). Pass a remote alias or htree:// URL."
            );
        }
    }
}

fn ensure_git_work_tree() -> Result<()> {
    let output = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .context("Failed to run git rev-parse --is-inside-work-tree")?;

    if !output.status.success() {
        anyhow::bail!("Current directory is not a git repository (work tree required)");
    }

    let inside_work_tree = String::from_utf8_lossy(&output.stdout).trim().eq("true");
    if !inside_work_tree {
        anyhow::bail!("Current directory is not a git repository (work tree required)");
    }

    Ok(())
}

/// Get current git branch name
fn git_current_branch() -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .context("Failed to run git rev-parse")?;

    if !output.status.success() {
        anyhow::bail!("Failed to determine current branch");
    }

    parse_current_branch_name(&String::from_utf8_lossy(&output.stdout))
}

fn parse_current_branch_name(stdout: &str) -> Result<String> {
    let branch = stdout.trim();
    if branch.is_empty() {
        anyhow::bail!("Failed to determine current branch");
    }
    if branch == "HEAD" {
        anyhow::bail!("Detached HEAD; pass --branch explicitly.");
    }
    Ok(branch.to_string())
}

/// Run git rev-parse on a ref
fn git_rev_parse(refspec: &str) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", refspec])
        .output()
        .context("Failed to run git rev-parse")?;

    if !output.status.success() {
        anyhow::bail!("Failed to resolve ref: {}", refspec);
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Run git rev-parse, returning None when the ref cannot be resolved.
fn git_rev_parse_opt(refspec: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["rev-parse", refspec])
        .output()
        .context("Failed to run git rev-parse")?;

    if !output.status.success() {
        return Ok(None);
    }

    Ok(Some(
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    ))
}

/// Run git rev-parse --abbrev-ref, returning None when the ref cannot be resolved.
fn git_rev_parse_abbrev_ref_opt(refspec: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", refspec])
        .output()
        .context("Failed to run git rev-parse --abbrev-ref")?;

    if !output.status.success() {
        return Ok(None);
    }

    Ok(Some(
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    ))
}

fn git_config_get_opt(key: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["config", "--get", key])
        .output()
        .context("Failed to run git config --get")?;

    if !output.status.success() {
        return Ok(None);
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        return Ok(None);
    }

    Ok(Some(value))
}

fn git_branch_upstream_remote_opt(branch: &str) -> Result<Option<String>> {
    let key = format!("branch.{}.remote", branch);
    git_config_get_opt(&key)
}

fn git_upstream_htree_remote_url_opt(branch: &str) -> Result<Option<String>> {
    let remote_name = match git_branch_upstream_remote_opt(branch)? {
        Some(remote_name) => remote_name,
        None => return Ok(None),
    };

    Ok(git_remote_get_url_opt(&remote_name)?.filter(|url| url.starts_with("htree://")))
}

fn git_remote_get_url_opt(remote: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["remote", "get-url", remote])
        .output()
        .context("Failed to run git remote get-url")?;

    if !output.status.success() {
        return Ok(None);
    }

    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if url.is_empty() {
        return Ok(None);
    }

    Ok(Some(url))
}

fn git_remote_names() -> Result<Vec<String>> {
    let output = Command::new("git")
        .arg("remote")
        .output()
        .context("Failed to run git remote")?;

    if !output.status.success() {
        anyhow::bail!("Failed to list git remotes");
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

fn git_htree_remotes() -> Result<Vec<(String, String)>> {
    let remotes = git_remote_names()?
        .into_iter()
        .map(|remote_name| {
            let remote = git_remote_get_url_opt(&remote_name)?
                .filter(|remote_url| remote_url.starts_with("htree://"))
                .map(|remote_url| (remote_name, remote_url));
            Ok(remote)
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(remotes.into_iter().flatten().collect())
}

/// Warn if a branch has no upstream, or if the upstream does not match the commit in the PR.
fn warn_if_branch_not_pushed(branch: &str, commit_tip: &str) {
    let upstream_ref = format!("{}@{{upstream}}", branch);

    let upstream_commit = match git_rev_parse_opt(&upstream_ref) {
        Ok(Some(commit)) => commit,
        Ok(None) => {
            eprintln!(
                "Warning: branch '{}' has no upstream tracking branch. Push it before creating a PR so the source branch can be fetched.",
                branch
            );
            return;
        }
        Err(err) => {
            eprintln!(
                "Warning: could not verify whether branch '{}' is pushed: {}",
                branch, err
            );
            return;
        }
    };

    if upstream_commit == commit_tip {
        return;
    }

    let upstream_name = git_rev_parse_abbrev_ref_opt(&upstream_ref)
        .ok()
        .flatten()
        .unwrap_or(upstream_ref);

    let local_short = commit_tip.get(..12).unwrap_or(commit_tip);
    let upstream_short = upstream_commit.get(..12).unwrap_or(&upstream_commit);
    eprintln!(
        "Warning: branch '{}' points to {} locally, but '{}' points to {}. Push before creating a PR if you want the latest commits included.",
        branch, local_short, upstream_name, upstream_short
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_pr_view_url_uses_git_host() {
        assert_eq!(
            build_pr_view_url("npub1target", "repo-name", "nevent1pullrequest"),
            "https://git.iris.to/#/npub1target/repo-name?tab=pulls&id=nevent1pullrequest"
        );
    }

    #[test]
    fn normalize_create_pr_params_applies_defaults() {
        let params = normalize_create_pr_params(CreatePrParamsInput {
            repo: None,
            title: "A Title",
            description: None,
            branch: None,
            target_branch: "master",
            clone_url: None,
        })
        .expect("normalize");

        assert_eq!(params.repo, RepoTargetSelection::InferFromGit);
        assert_eq!(params.branch, SourceBranchSelection::CurrentBranch);
        assert_eq!(params.clone_url, CloneUrlSelection::DefaultFromSelfAndRepo);
        assert_eq!(params.title, "A Title");
        assert_eq!(params.description, "");
        assert!(!params.description_was_provided);
        assert_eq!(params.target_branch, "master");
    }

    #[test]
    fn normalize_create_pr_params_preserves_explicit_values() {
        let params = normalize_create_pr_params(CreatePrParamsInput {
            repo: Some("npub1abc/repo"),
            title: "Title",
            description: Some("desc"),
            branch: Some("feature"),
            target_branch: "main",
            clone_url: Some("htree://self/repo"),
        })
        .expect("normalize");

        assert_eq!(
            params.repo,
            RepoTargetSelection::Explicit("npub1abc/repo".to_string())
        );
        assert_eq!(
            params.branch,
            SourceBranchSelection::Explicit("feature".to_string())
        );
        assert_eq!(
            params.clone_url,
            CloneUrlSelection::Explicit("htree://self/repo".to_string())
        );
        assert_eq!(params.title, "Title");
        assert_eq!(params.description, "desc");
        assert!(params.description_was_provided);
        assert_eq!(params.target_branch, "main");
    }

    #[test]
    fn normalize_create_pr_params_trims_selector_like_fields() {
        let params = normalize_create_pr_params(CreatePrParamsInput {
            repo: Some("  team/htree  "),
            title: "  Title  ",
            description: Some("desc"),
            branch: Some("  feature  "),
            target_branch: "  master  ",
            clone_url: Some("  htree://self/repo  "),
        })
        .expect("normalize");

        assert_eq!(
            params.repo,
            RepoTargetSelection::Explicit("team/htree".to_string())
        );
        assert_eq!(params.title, "Title");
        assert_eq!(
            params.branch,
            SourceBranchSelection::Explicit("feature".to_string())
        );
        assert_eq!(params.target_branch, "master");
        assert_eq!(
            params.clone_url,
            CloneUrlSelection::Explicit("htree://self/repo".to_string())
        );
    }

    #[test]
    fn normalize_create_pr_params_empty_selector_strings_fall_back_to_defaults() {
        let params = normalize_create_pr_params(CreatePrParamsInput {
            repo: Some("   "),
            title: "Title",
            description: None,
            branch: Some(""),
            target_branch: "master",
            clone_url: Some(" "),
        })
        .expect("normalize");

        assert_eq!(params.repo, RepoTargetSelection::InferFromGit);
        assert_eq!(params.branch, SourceBranchSelection::CurrentBranch);
        assert_eq!(params.clone_url, CloneUrlSelection::DefaultFromSelfAndRepo);
    }

    #[test]
    fn normalize_create_pr_params_rejects_empty_title() {
        for title in ["", "   "] {
            let err = normalize_create_pr_params(CreatePrParamsInput {
                repo: None,
                title,
                description: None,
                branch: None,
                target_branch: "master",
                clone_url: None,
            })
            .expect_err("empty title should fail");
            assert!(format!("{err}").contains("PR title cannot be empty"));
        }
    }

    #[test]
    fn normalize_create_pr_params_preserves_description_whitespace() {
        let params = normalize_create_pr_params(CreatePrParamsInput {
            repo: None,
            title: "Title",
            description: Some("  hello\n"),
            branch: None,
            target_branch: "master",
            clone_url: None,
        })
        .expect("normalize");

        assert_eq!(params.description, "  hello\n");
        assert!(params.description_was_provided);
    }

    #[test]
    fn normalize_create_pr_params_defaults_empty_target_branch_to_master() {
        for target_branch in ["", "   "] {
            let params = normalize_create_pr_params(CreatePrParamsInput {
                repo: None,
                title: "Title",
                description: None,
                branch: None,
                target_branch,
                clone_url: None,
            })
            .expect("normalize");
            assert_eq!(params.target_branch, "master");
        }
    }

    #[test]
    fn parse_repo_target_strips_fragment_from_repo_name() {
        let keys = Keys::generate();
        let pubkey_hex = hex::encode(keys.public_key().to_bytes());
        let input = format!("htree://{pubkey_hex}/repo-name#k=secret");

        let (parsed_pubkey, repo_name) = parse_repo_target(&input).expect("parse target");

        assert_eq!(parsed_pubkey, pubkey_hex);
        assert_eq!(repo_name, "repo-name");
    }

    #[test]
    fn parse_current_branch_name_rejects_detached_head() {
        let err = parse_current_branch_name("HEAD\n").expect_err("detached head should fail");
        let msg = format!("{err}");
        assert!(msg.contains("Detached HEAD"));
        assert!(msg.contains("--branch"));
    }

    #[test]
    fn pr_list_state_to_filter_maps_all_values() {
        assert_eq!(PrListState::Open.to_filter(), PullRequestStateFilter::Open);
        assert_eq!(
            PrListState::Applied.to_filter(),
            PullRequestStateFilter::Applied
        );
        assert_eq!(
            PrListState::Closed.to_filter(),
            PullRequestStateFilter::Closed
        );
        assert_eq!(
            PrListState::Draft.to_filter(),
            PullRequestStateFilter::Draft
        );
        assert_eq!(PrListState::All.to_filter(), PullRequestStateFilter::All);
    }

    #[test]
    fn resolve_list_repo_target_input_accepts_explicit_repo_without_git() {
        let out = resolve_list_repo_target_input(Some("htree://npub1test/repo")).expect("resolve");
        assert_eq!(out, "htree://npub1test/repo");

        let out = resolve_list_repo_target_input(Some("npub1test/repo")).expect("resolve");
        assert_eq!(out, "npub1test/repo");
    }
}
