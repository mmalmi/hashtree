use super::*;

pub(super) enum Transit {
    Local(BoundFipsEndpoint, String),
    #[cfg(feature = "public-provider-stress")]
    Process(ProcessTransit),
}

impl Transit {
    pub(super) async fn local(scope: &str) -> Self {
        let (endpoint, address) = udp_endpoint(scope).await;
        Self::Local(endpoint, address)
    }

    #[cfg(feature = "public-provider-stress")]
    pub(super) async fn process(scope: &str) -> Result<Self> {
        Ok(Self::Process(ProcessTransit::start(scope).await?))
    }

    pub(super) fn npub(&self) -> &str {
        match self {
            Self::Local(endpoint, _) => &endpoint.local_peer_id,
            #[cfg(feature = "public-provider-stress")]
            Self::Process(process) => &process.npub,
        }
    }

    pub(super) fn address(&self) -> &str {
        match self {
            Self::Local(_, address) => address,
            #[cfg(feature = "public-provider-stress")]
            Self::Process(process) => &process.address,
        }
    }

    pub(super) async fn configure(&mut self, peers: Vec<FipsPeerConfig>) -> Result<()> {
        match self {
            Self::Local(endpoint, _) => {
                set_fips_peer_configs(&endpoint.native_endpoint, peers).await?;
            }
            #[cfg(feature = "public-provider-stress")]
            Self::Process(process) => process.configure(peers).await?,
        }
        Ok(())
    }

    pub(super) async fn shutdown(&mut self) -> Result<()> {
        match self {
            Self::Local(endpoint, _) => endpoint.native_endpoint.shutdown().await?,
            #[cfg(feature = "public-provider-stress")]
            Self::Process(process) => process.shutdown().await?,
        }
        Ok(())
    }
}

#[cfg(feature = "public-provider-stress")]
pub(super) struct ProcessTransit {
    npub: String,
    address: String,
    child: tokio::process::Child,
    input: Option<tokio::process::ChildStdin>,
    output: tokio::io::BufReader<tokio::process::ChildStdout>,
}

#[cfg(feature = "public-provider-stress")]
impl ProcessTransit {
    async fn start(scope: &str) -> Result<Self> {
        let binary = std::env::var_os("HTREE_TEST_TRANSIT_BIN").context(
            "run scripts/test_public_provider_admission.py with the fixed-core transit binary",
        )?;
        let mut child = tokio::process::Command::new(binary)
            .arg(scope)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let mut process = Self {
            npub: String::new(),
            address: String::new(),
            input: child.stdin.take(),
            output: tokio::io::BufReader::new(child.stdout.take().unwrap()),
            child,
        };
        let ready = process.read_response().await?;
        anyhow::ensure!(ready["forwardMinIntervalSecs"] == 2);
        anyhow::ensure!(ready["maxPeers"] == 18);
        process.npub = ready["npub"]
            .as_str()
            .context("missing transit identity")?
            .into();
        process.address = ready["udpAddress"]
            .as_str()
            .context("missing transit address")?
            .into();
        let address: std::net::SocketAddr = process.address.parse()?;
        anyhow::ensure!(address.ip().is_loopback() && address.port() != 0);
        Ok(process)
    }

    async fn configure(&mut self, peers: Vec<FipsPeerConfig>) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let peers = peers
            .into_iter()
            .map(|peer| serde_json::json!({"npub": peer.npub, "udp_addresses": peer.udp_addresses}))
            .collect::<Vec<_>>();
        let command = serde_json::to_string(&serde_json::json!({"peers": peers}))? + "\n";
        let input = self.input.as_mut().context("transit already stopped")?;
        input.write_all(command.as_bytes()).await?;
        input.flush().await?;
        anyhow::ensure!(self.read_response().await?["updated"] == true);
        Ok(())
    }

    async fn read_response(&mut self) -> Result<serde_json::Value> {
        use tokio::io::AsyncBufReadExt;
        let mut line = String::new();
        let size = timeout(Duration::from_secs(10), self.output.read_line(&mut line))
            .await
            .context("transit response timed out")??;
        anyhow::ensure!(size > 0, "transit exited before responding");
        Ok(serde_json::from_str(&line)?)
    }

    async fn shutdown(&mut self) -> Result<()> {
        drop(self.input.take());
        let status = timeout(Duration::from_secs(10), self.child.wait())
            .await
            .context("transit shutdown timed out")??;
        anyhow::ensure!(status.success(), "transit failed: {status}");
        Ok(())
    }
}
