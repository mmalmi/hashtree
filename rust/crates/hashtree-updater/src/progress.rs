use std::sync::Arc;

/// Streaming download progress callbacks.
///
/// Lifecycle: `Started { content_length }` → 0..N `Progress { chunk_len }` →
/// `Finished`. `content_length` may be `None` if the asset's chunked tree
/// did not record a top-level size.
#[derive(Debug, Clone)]
pub enum DownloadEvent {
    Started { content_length: Option<u64> },
    Progress { chunk_len: u64, downloaded: u64 },
    Finished { total: u64 },
}

/// Convenience callback alias. `Send + Sync` so plugin transports (Tauri
/// channels, MPSC) can wrap and forward it.
pub type DownloadCallback = Arc<dyn Fn(DownloadEvent) + Send + Sync>;
