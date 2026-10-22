use crate::{Error, Result};
use std::{net::SocketAddr, time::Duration};

pub const MAX_METADATA_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_READ_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_FILES: usize = 100_000;

/// Constructing options does not open sockets. Discovery has no implicit DNS bootstrap.
#[derive(Clone, Debug)]
pub struct NetworkOptions {
    pub listen: SocketAddr,
    pub peers: Vec<SocketAddr>,
    pub dht: bool,
    pub bootstrap: Vec<SocketAddr>,
    pub trackers: bool,
    pub cache_bytes: usize,
    pub timeout: Duration,
    pub connections: u16,
    /// Bytes per second; zero means unlimited.
    pub upload_limit: u32,
    pub download_limit: u32,
}

impl Default for NetworkOptions {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:0".parse().unwrap(),
            peers: Vec::new(),
            dht: false,
            bootstrap: Vec::new(),
            trackers: false,
            cache_bytes: 64 * 1024 * 1024,
            timeout: Duration::from_secs(60),
            connections: 40,
            upload_limit: 0,
            download_limit: 0,
        }
    }
}

impl NetworkOptions {
    pub fn validate(&self) -> Result<()> {
        if self.cache_bytes < 16 * 1024 || self.cache_bytes > 1024 * 1024 * 1024 {
            return Err(Error::Invalid(
                "piece cache must be between 16 KiB and 1 GiB".into(),
            ));
        }
        if self.timeout < Duration::from_millis(1) || self.timeout > Duration::from_secs(3600) {
            return Err(Error::Invalid(
                "timeout must be between 1 ms and one hour".into(),
            ));
        }
        if self.connections == 0 || self.connections > 1000 {
            return Err(Error::Invalid(
                "connections must be between 1 and 1000".into(),
            ));
        }
        if self.peers.len() > 1000 || self.bootstrap.len() > 100 {
            return Err(Error::Invalid(
                "too many peer or bootstrap addresses".into(),
            ));
        }
        if self
            .peers
            .iter()
            .chain(&self.bootstrap)
            .any(|p| p.port() == 0 || p.ip().is_unspecified() || p.ip().is_multicast())
        {
            return Err(Error::Invalid(
                "peers and bootstrap nodes need unicast addresses and nonzero ports".into(),
            ));
        }
        if !self.dht && !self.bootstrap.is_empty() {
            return Err(Error::Invalid(
                "bootstrap nodes require DHT to be enabled".into(),
            ));
        }
        if self.upload_limit > i32::MAX as u32 || self.download_limit > i32::MAX as u32 {
            return Err(Error::Invalid("bandwidth limit is too large".into()));
        }
        Ok(())
    }
}

pub(crate) fn validate_component(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 255
        || name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':'))
    {
        return Err(Error::Invalid(
            "unsafe or empty torrent path component".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_path(path: &str) -> Result<()> {
    if path.len() > 4096 {
        return Err(Error::Invalid("torrent path is too long".into()));
    }
    for part in path.split('/') {
        validate_component(part)?;
    }
    Ok(())
}
