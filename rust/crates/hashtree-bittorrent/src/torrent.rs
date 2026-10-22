pub use crate::ffi::Stats;
use crate::{
    ffi::{self, Native, RawMetadata},
    Error, File, NetworkOptions, Result, Snapshot, MAX_METADATA_BYTES, MAX_READ_BYTES,
};
use futures::TryStreamExt;
use hashtree_core::{Cid, HashTree, HashTreeConfig, Store};
use std::sync::{atomic::AtomicBool, Arc};
use tokio::sync::Mutex;

#[derive(Clone, Copy, Debug, Default)]
pub enum Format {
    V1,
    V2,
    #[default]
    Hybrid,
}

#[derive(Clone)]
pub struct Metadata {
    pub(crate) raw: Arc<RawMetadata>,
    files: Arc<Vec<File>>,
    snapshot: Option<Snapshot>,
}
impl Metadata {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() || bytes.len() > MAX_METADATA_BYTES {
            return Err(Error::Invalid(
                "torrent metadata exceeds size limit or is empty".into(),
            ));
        }
        Self::from_raw(RawMetadata::parse(bytes)?, None)
    }
    fn from_raw(raw: RawMetadata, snapshot: Option<Snapshot>) -> Result<Self> {
        let files = Arc::new(raw.files()?);
        Ok(Self {
            raw: Arc::new(raw),
            files,
            snapshot,
        })
    }
    /// Hash a snapshot without opening network sockets or writing a second copy.
    pub async fn create(snapshot: Snapshot, format: Format, piece_length: usize) -> Result<Self> {
        if !(16 * 1024..=MAX_READ_BYTES).contains(&piece_length) || !piece_length.is_power_of_two()
        {
            return Err(Error::Invalid(
                "piece size must be a power of two between 16 KiB and 16 MiB".into(),
            ));
        }
        let runtime = tokio::runtime::Handle::current();
        let cancel = Arc::new(AtomicBool::new(false));
        let _guard = ffi::CancelOnDrop(cancel.clone());
        tokio::task::spawn_blocking(move || {
            let format = match format {
                Format::V1 => 1,
                Format::V2 => 2,
                Format::Hybrid => 0,
            };
            let raw =
                RawMetadata::create(&snapshot, format, piece_length as i32, runtime, &cancel)?;
            Self::from_raw(raw, Some(snapshot))
        })
        .await?
    }
    pub fn files(&self) -> &[File] {
        &self.files
    }
    /// Magnet-discovered metadata may not yet contain every v2 piece layer.
    /// Use the original magnet until a complete torrent file is available.
    pub fn torrent_bytes(&self) -> Result<Vec<u8>> {
        let bytes = self.raw.bytes();
        if bytes.is_empty() {
            Err(Error::Invalid(
                "use the source magnet; complete torrent metadata is not available".into(),
            ))
        } else {
            Ok(bytes)
        }
    }
    pub fn magnet(&self) -> Result<String> {
        self.raw.magnet()
    }
    pub fn piece_length(&self) -> usize {
        self.raw.piece_length()
    }
    pub fn file(&self, index: usize) -> Result<&File> {
        self.files
            .iter()
            .find(|f| f.index == index)
            .ok_or_else(|| Error::Invalid("unknown torrent file".into()))
    }
}

/// A demand-driven reader. Only requested pieces are downloaded; memory is bounded
/// by the configured cache plus the current read and libtorrent's network buffers.
#[derive(Clone)]
pub struct Torrent {
    pub(crate) engine: Arc<Mutex<Native>>,
    metadata: Metadata,
    timeout_ms: u64,
}
impl Torrent {
    pub async fn open(metadata: Metadata, options: NetworkOptions) -> Result<Self> {
        options.validate()?;
        let runtime = tokio::runtime::Handle::current();
        let raw = metadata.raw.clone();
        let timeout_ms = options.timeout.as_millis() as u64;
        let engine = tokio::task::spawn_blocking(move || {
            Native::open(Some(&raw), None, None, &options, runtime)
        })
        .await??;
        Ok(Self {
            engine: Arc::new(Mutex::new(engine)),
            metadata,
            timeout_ms,
        })
    }
    pub async fn magnet(magnet: &str, options: NetworkOptions) -> Result<Self> {
        if !magnet.starts_with("magnet:?") || magnet.len() > 64 * 1024 {
            return Err(Error::Invalid("invalid magnet URI".into()));
        }
        options.validate()?;
        let runtime = tokio::runtime::Handle::current();
        let magnet = magnet.to_owned();
        let timeout_ms = options.timeout.as_millis() as u64;
        let engine = tokio::task::spawn_blocking(move || {
            Native::open(None, Some(&magnet), None, &options, runtime)
        })
        .await??;
        let engine = Arc::new(Mutex::new(engine));
        let raw = ffi::invoke(engine.clone(), move |e, c| e.metadata(timeout_ms, c)).await?;
        Ok(Self {
            engine,
            metadata: Metadata::from_raw(raw, None)?,
            timeout_ms,
        })
    }
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }
    pub async fn connect(&self, peer: std::net::SocketAddr) -> Result<()> {
        ffi::invoke(self.engine.clone(), move |e, _| e.connect(peer)).await
    }
    pub async fn stats(&self) -> Result<Stats> {
        ffi::invoke(self.engine.clone(), |e, _| e.stats()).await
    }
    pub async fn read_range(&self, file: usize, offset: u64, len: usize) -> Result<Vec<u8>> {
        let size = self.metadata.file(file)?.size;
        if len > MAX_READ_BYTES || offset > size || len as u64 > size - offset {
            return Err(Error::Invalid("range exceeds file or read limit".into()));
        }
        if len == 0 {
            return Ok(Vec::new());
        }
        let timeout = self.timeout_ms;
        ffi::invoke(self.engine.clone(), move |e, c| {
            e.read(file, offset, len, timeout, c)
        })
        .await
    }
    /// Import one selected file using normal hashtree chunking and encryption.
    pub async fn materialize<S: Store>(
        &self,
        file: usize,
        config: HashTreeConfig<S>,
    ) -> Result<(Cid, u64)> {
        let size = self.metadata.file(file)?.size;
        let torrent = self.clone();
        let stream =
            futures::stream::try_unfold((torrent, 0u64), move |(torrent, offset)| async move {
                if offset == size {
                    return Ok::<_, std::io::Error>(None);
                }
                let len = (size - offset).min(2 * 1024 * 1024) as usize;
                let bytes = torrent
                    .read_range(file, offset, len)
                    .await
                    .map_err(std::io::Error::other)?;
                Ok(Some((bytes, (torrent, offset + len as u64))))
            })
            .into_async_read();
        futures::pin_mut!(stream);
        Ok(HashTree::new(config).put_stream(stream).await?)
    }
}

/// Seeding can only start with metadata hashed from an explicitly selected snapshot.
pub struct Seed {
    engine: Arc<Mutex<Native>>,
    metadata: Metadata,
}
impl Seed {
    pub async fn start(metadata: Metadata, options: NetworkOptions) -> Result<Self> {
        options.validate()?;
        let source = metadata.snapshot.clone().ok_or_else(|| {
            Error::Invalid("seeding requires metadata created from a verified snapshot".into())
        })?;
        let runtime = tokio::runtime::Handle::current();
        let raw = metadata.raw.clone();
        let native = tokio::task::spawn_blocking(move || {
            Native::open(Some(&raw), None, Some(source), &options, runtime)
        })
        .await??;
        Ok(Self {
            engine: Arc::new(Mutex::new(native)),
            metadata,
        })
    }
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }
    pub async fn stats(&self) -> Result<Stats> {
        ffi::invoke(self.engine.clone(), |e, _| e.stats()).await
    }
    pub async fn connect(&self, peer: std::net::SocketAddr) -> Result<()> {
        ffi::invoke(self.engine.clone(), move |e, _| e.connect(peer)).await
    }
}
