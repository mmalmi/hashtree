use anyhow::{ensure, Result};
use hashtree_config::Config;
use std::time::Duration;

#[derive(Clone)]
pub struct ClientConfig {
    pub daemon_url: Option<String>,
    pub local_only: bool,
    pub relays: Vec<String>,
    pub read_servers: Vec<String>,
    pub resolve_window: Duration,
    pub request_timeout: Duration,
}

fn flag(name: &str) -> Option<bool> {
    std::env::var(name).ok().map(|v| {
        !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    })
}

impl ClientConfig {
    /// Read the shared config without creating an identity or writing defaults.
    pub fn from_env() -> Result<Self> {
        let path = hashtree_config::get_config_path();
        let config: Config = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(e.into()),
        };
        let local_only = flag("HTREE_LOCAL_DAEMON_ONLY").unwrap_or(false);
        let prefer_local = local_only || flag("HTREE_PREFER_LOCAL_DAEMON").unwrap_or(true);
        let port = config
            .server
            .bind_address
            .rsplit_once(':')
            .and_then(|(_, p)| p.parse::<u16>().ok())
            .unwrap_or(8080);
        let daemon_url = prefer_local.then(|| {
            std::env::var("HTREE_DAEMON_URL").unwrap_or_else(|_| format!("http://127.0.0.1:{port}"))
        });
        if let Some(value) = &daemon_url {
            let url = reqwest::Url::parse(value)?;
            ensure!(
                url.scheme() == "http"
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && url.path() == "/"
                    && url.host_str().is_some_and(|h| h == "localhost"
                        || h.parse::<std::net::IpAddr>()
                            .is_ok_and(|ip| ip.is_loopback())),
                "HTREE_DAEMON_URL must be a loopback HTTP origin"
            );
        }
        let relays = std::env::var("NOSTR_RELAYS")
            .ok()
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or(config.nostr.relays);
        let mut read_servers = config.blossom.read_servers;
        read_servers.extend(config.blossom.servers);
        Ok(Self {
            daemon_url: daemon_url.map(|s| s.trim_end_matches('/').to_string()),
            local_only,
            relays,
            read_servers,
            resolve_window: Duration::from_secs(3),
            request_timeout: Duration::from_secs(30),
        })
    }
}
