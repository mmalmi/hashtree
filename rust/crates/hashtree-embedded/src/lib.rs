use anyhow::{Context, Result};
use hashtree_cli::daemon::{EmbeddedDaemonInfo, EmbeddedDaemonOptions};
use hashtree_cli::Config;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct HostDaemonOptions {
    pub state_root: PathBuf,
    pub bind_address: String,
}

impl HostDaemonOptions {
    pub fn new(state_root: impl Into<PathBuf>) -> Self {
        Self {
            state_root: state_root.into(),
            bind_address: "127.0.0.1:0".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct HostDaemonStatus {
    pub base_url: String,
    pub self_npub: String,
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
}

pub struct HostDaemonRuntime {
    runtime: tokio::runtime::Runtime,
    info: EmbeddedDaemonInfo,
    config_dir: PathBuf,
    data_dir: PathBuf,
}

impl HostDaemonRuntime {
    pub fn start(options: HostDaemonOptions) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("build embedded host runtime")?;

        let config_dir = options.state_root.join("config");
        let data_dir = options.state_root.join("data");
        std::fs::create_dir_all(&config_dir).context("create embedded config dir")?;
        std::fs::create_dir_all(&data_dir).context("create embedded data dir")?;

        let config = browser_config(&data_dir);
        let info = runtime
            .block_on(hashtree_cli::daemon::start_embedded(
                EmbeddedDaemonOptions {
                    config,
                    data_dir: data_dir.clone(),
                    config_dir: Some(config_dir.clone()),
                    bind_address: options.bind_address,
                    relays: None,
                    extra_routes: None,
                    cors: None,
                },
            ))
            .context("start embedded hashtree daemon")?;

        Ok(Self {
            runtime,
            info,
            config_dir,
            data_dir,
        })
    }

    pub fn status(&self) -> HostDaemonStatus {
        HostDaemonStatus {
            base_url: format!("http://{}", self.info.addr),
            self_npub: self.info.npub.clone(),
            config_dir: self.config_dir.clone(),
            data_dir: self.data_dir.clone(),
        }
    }

    pub fn base_url(&self) -> String {
        self.status().base_url
    }

    pub fn self_npub(&self) -> &str {
        &self.info.npub
    }

    pub fn shutdown(&mut self) {
        let controller = self.info.daemon_controller.clone();
        self.runtime.block_on(async move {
            controller.shutdown().await;
        });
    }
}

impl Drop for HostDaemonRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn browser_config(data_dir: &Path) -> Config {
    let mut config = Config::default();
    config.storage.data_dir = data_dir.to_string_lossy().to_string();
    config.server.enable_auth = false;
    config.server.public_writes = false;
    config.server.enable_webrtc = false;
    config.server.enable_multicast = false;
    config.server.max_multicast_peers = 0;
    config.server.enable_bluetooth = false;
    config.server.max_bluetooth_peers = 0;
    config.server.stun_port = 0;
    config.sync.enabled = false;
    config.nostr.relays.clear();
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use nostr::{EventBuilder, Keys, Kind, Tag, TagKind, Timestamp};
    use reqwest::blocking::Client;
    use tempfile::TempDir;

    fn create_blossom_auth(keys: &Keys, action: &str) -> String {
        let expiration = Timestamp::from(Timestamp::now().as_u64() + 300);
        let tags = vec![
            Tag::custom(TagKind::Custom("t".into()), vec![action.to_string()]),
            Tag::custom(
                TagKind::Custom("expiration".into()),
                vec![expiration.to_string()],
            ),
        ];
        let event = EventBuilder::new(Kind::Custom(24242), "", tags)
            .to_event(keys)
            .expect("sign blossom auth");
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_string(&event).expect("serialize auth event"));
        format!("Nostr {encoded}")
    }

    #[test]
    fn host_runtime_starts_and_shuts_down() {
        let temp = TempDir::new().expect("temp dir");
        let mut runtime =
            HostDaemonRuntime::start(HostDaemonOptions::new(temp.path())).expect("start daemon");

        let status = runtime.status();
        assert!(
            status.config_dir.join("keys").exists(),
            "expected host daemon to materialize keys in its config dir"
        );

        let response = reqwest::blocking::get(format!("{}/htree/test", status.base_url))
            .expect("fetch test endpoint");
        assert!(
            response.status().is_success(),
            "embedded daemon should answer"
        );

        runtime.shutdown();

        let stopped = reqwest::blocking::get(format!("{}/htree/test", status.base_url)).is_err();
        assert!(stopped, "expected host daemon shutdown to stop serving");
    }

    #[test]
    fn host_runtime_rejects_public_blossom_uploads() {
        let temp = TempDir::new().expect("temp dir");
        let runtime =
            HostDaemonRuntime::start(HostDaemonOptions::new(temp.path())).expect("start daemon");
        let status = runtime.status();

        let keys = Keys::generate();
        let response = Client::new()
            .put(format!("{}/upload", status.base_url))
            .header("Authorization", create_blossom_auth(&keys, "upload"))
            .header("Content-Type", "text/plain")
            .body("browser-mode upload probe")
            .send()
            .expect("upload request");

        assert_eq!(
            response.status(),
            reqwest::StatusCode::FORBIDDEN,
            "browser-mode daemon must not accept public Blossom uploads"
        );
    }
}
