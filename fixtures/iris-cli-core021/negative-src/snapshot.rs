use anyhow::{Context, Result, ensure};
use hashtree_core::{sha256, to_hex};
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

pub const DEFAULT_IDENTITY: &str =
    "npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm";
// Canonical company-root bootstrap, captured 2026-10-29 and maintained by
// nostr-social-graph. Numeric aliases are normalized before projection.
// Updating this checksum pin is a release.
pub const DEFAULT_URL: &str =
    "https://github.com/mmalmi/nostr-social-graph/releases/download/v3.0.1/socialGraph.bin";
pub const DEFAULT_SHA256: &str = "df349a23122993183b9b9d2d056b7c655a984bdc86bc72f747202ad772de4d8f";

pub fn cache_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .context("set XDG_CACHE_HOME or specify --graph/--output")?;
    Ok(base.join("iris-nostr/social-graph.bin"))
}

pub fn validate(
    bytes: &[u8],
    digest: Option<&str>,
    identity: &str,
) -> Result<nostr_social_graph::SocialGraph> {
    ensure!(
        bytes.len() as u64 <= crate::io::MAX_INPUT,
        "snapshot exceeds 128 MiB"
    );
    if let Some(expected) = digest {
        ensure!(
            to_hex(&sha256(bytes)).eq_ignore_ascii_case(expected),
            "snapshot SHA-256 mismatch"
        );
    }
    let root = crate::graph::public_key(identity)?.to_hex();
    let mut graph = nostr_social_graph::SocialGraph::from_binary(&root, bytes)
        .context("invalid graph snapshot")?;
    graph.recalculate_follow_distances();
    Ok(graph)
}

pub async fn fetch(
    url: Option<&str>,
    digest: Option<&str>,
    output: &Path,
    identity: &str,
) -> Result<serde_json::Value> {
    ensure!(
        output.extension().is_some_and(|ext| ext == "bin"),
        "snapshot output must end in .bin"
    );
    let expected = digest.or_else(|| url.is_none().then_some(DEFAULT_SHA256));
    let url = reqwest::Url::parse(url.unwrap_or(DEFAULT_URL))?;
    let local = url.host_str().is_some_and(|host| {
        host.trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    });
    ensure!(
        (url.scheme() == "https" || (url.scheme() == "http" && local))
            && url.username().is_empty()
            && url.password().is_none(),
        "use HTTPS or loopback HTTP without credentials"
    );
    // GitHub release downloads redirect to their asset host. Keep custom snapshot
    // endpoints redirect-free and restrict this exception to the pinned default.
    let redirects = if url.as_str() == DEFAULT_URL {
        reqwest::redirect::Policy::custom(|attempt| {
            let target = attempt.url();
            if attempt.previous().len() <= 3
                && target.scheme() == "https"
                && target.host_str() == Some("release-assets.githubusercontent.com")
                && target.username().is_empty()
                && target.password().is_none()
            {
                attempt.follow()
            } else {
                attempt.stop()
            }
        })
    } else {
        reqwest::redirect::Policy::none()
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .redirect(redirects)
        .no_proxy()
        .build()?;
    let mut response = client.get(url.clone()).send().await?.error_for_status()?;
    ensure!(
        response.status().is_success(),
        "snapshot server returned a redirect"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= crate::io::MAX_INPUT as usize,
            "snapshot exceeds 128 MiB"
        );
        bytes.extend_from_slice(&chunk);
    }
    let graph = validate(&bytes, expected, identity)?;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    staged.write_all(&bytes)?;
    staged.as_file().sync_all()?;
    staged
        .persist(output)
        .context("save verified graph snapshot")?;
    Ok(
        serde_json::json!({"path":output,"bytes":bytes.len(),"sha256":to_hex(&sha256(&bytes)),
        "digest_pinned":expected.is_some(),"identity":graph.get_root(),
        "direct_follows":graph.get_followed_by_user(graph.get_root()).len(),
        "authors_within_two_hops":graph.users_in_distance_order(Some(2)).len(),
        "authors_within_three_hops":graph.users_in_distance_order(Some(3)).len(),
        "source":url.as_str(),"trust":"binary graph snapshot, not individually signed events"}),
    )
}
