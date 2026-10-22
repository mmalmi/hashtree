//! The C boundary owns all libtorrent objects. Native exceptions are converted to
//! thread-local errors; callbacks catch Rust panics. Sessions serialize calls and
//! join their disk/network workers before releasing the source callback context.
use crate::{Error, File, NetworkOptions, Result, Snapshot};
use std::{
    ffi::{c_char, c_void, CStr, CString},
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

#[repr(C)]
pub(crate) struct NativeFile {
    path: *const c_char,
    size: u64,
}
#[repr(C)]
struct Options {
    listen: *const c_char,
    bootstrap: *const c_char,
    cache_bytes: u64,
    connections: u32,
    upload_limit: u32,
    download_limit: u32,
    dht: u8,
    trackers: u8,
}
#[repr(C)]
#[derive(Default, Clone, Debug, serde::Serialize)]
pub struct Stats {
    pub downloaded: u64,
    pub uploaded: u64,
    pub cache_bytes: u64,
    pub peers: u32,
    pub pieces: u32,
    pub port: u16,
}
type ReadFn = unsafe extern "C" fn(*mut c_void, u32, u64, u32, *mut c_char) -> i32;
type CancelFn = unsafe extern "C" fn(*mut c_void) -> i32;

extern "C" {
    fn ht_error() -> *const c_char;
    fn ht_error_kind() -> i32;
    fn ht_meta_parse(data: *const c_char, len: usize) -> *mut c_void;
    fn ht_meta_create(
        files: *const NativeFile,
        count: usize,
        format: i32,
        piece: i32,
        read: ReadFn,
        ctx: *mut c_void,
        cancel: CancelFn,
        cancel_ctx: *mut c_void,
    ) -> *mut c_void;
    fn ht_meta_free(ptr: *mut c_void);
    fn ht_meta_size(ptr: *const c_void) -> usize;
    fn ht_meta_bytes(ptr: *const c_void) -> *const c_char;
    fn ht_meta_file_count(ptr: *const c_void) -> i32;
    fn ht_meta_file_path(ptr: *const c_void, index: i32) -> *const c_char;
    fn ht_meta_file_size(ptr: *const c_void, index: i32) -> u64;
    fn ht_meta_file_flags(ptr: *const c_void, index: i32) -> i32;
    fn ht_meta_piece_length(ptr: *const c_void) -> i32;
    fn ht_meta_magnet(ptr: *const c_void) -> *const c_char;
    fn ht_session_open(
        meta: *const c_void,
        opts: *const Options,
        files: *const NativeFile,
        count: usize,
        read: Option<ReadFn>,
        ctx: *mut c_void,
    ) -> *mut c_void;
    fn ht_session_magnet(magnet: *const c_char, opts: *const Options) -> *mut c_void;
    fn ht_session_metadata(
        ptr: *mut c_void,
        timeout: u64,
        cancel: CancelFn,
        ctx: *mut c_void,
    ) -> *mut c_void;
    fn ht_session_free(ptr: *mut c_void);
    fn ht_session_connect(ptr: *mut c_void, ip: *const c_char, port: u16) -> i32;
    fn ht_session_read(
        ptr: *mut c_void,
        file: i32,
        offset: u64,
        len: u32,
        data: *mut c_char,
        timeout: u64,
        cancel: CancelFn,
        ctx: *mut c_void,
    ) -> i32;
    fn ht_session_stats(ptr: *mut c_void, stats: *mut Stats) -> i32;
}

fn last_error() -> Error {
    // SAFETY: the native error string is thread-local, valid until the next call.
    unsafe {
        match ht_error_kind() {
            1 => Error::Timeout,
            2 => Error::Cancelled,
            _ => Error::Engine(CStr::from_ptr(ht_error()).to_string_lossy().into_owned()),
        }
    }
}
fn check(code: i32) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(last_error())
    }
}
fn string(ptr: *const c_char) -> Result<String> {
    // SAFETY: callers pass a live native metadata string.
    unsafe {
        CStr::from_ptr(ptr)
            .to_str()
            .map(str::to_owned)
            .map_err(|_| Error::Invalid("torrent paths must be UTF-8".into()))
    }
}

pub(crate) struct RawMetadata(*mut c_void);
// SAFETY: metadata is immutable after construction; getters only read it.
unsafe impl Send for RawMetadata {}
unsafe impl Sync for RawMetadata {}
impl Drop for RawMetadata {
    fn drop(&mut self) {
        unsafe { ht_meta_free(self.0) }
    }
}
impl RawMetadata {
    fn from_ptr(ptr: *mut c_void) -> Result<Self> {
        if ptr.is_null() {
            Err(last_error())
        } else {
            Ok(Self(ptr))
        }
    }
    pub fn parse(data: &[u8]) -> Result<Self> {
        Self::from_ptr(unsafe { ht_meta_parse(data.as_ptr().cast(), data.len()) })
    }
    pub fn create(
        snapshot: &Snapshot,
        format: i32,
        piece: i32,
        runtime: tokio::runtime::Handle,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        let files = FileArray::new(snapshot.files())?;
        let mut source = SourceBridge {
            snapshot: snapshot.clone(),
            runtime,
        };
        Self::from_ptr(unsafe {
            ht_meta_create(
                files.native.as_ptr(),
                files.native.len(),
                format,
                piece,
                read,
                (&mut source as *mut SourceBridge).cast(),
                cancelled,
                cancel_ptr(cancel),
            )
        })
    }
    pub fn files(&self) -> Result<Vec<File>> {
        let mut files = Vec::new();
        // SAFETY: each index is in the metadata's validated file range.
        unsafe {
            for index in 0..ht_meta_file_count(self.0) {
                if ht_meta_file_flags(self.0, index) != 0 {
                    continue;
                }
                let path = string(ht_meta_file_path(self.0, index))?;
                crate::config::validate_path(&path)?;
                files.push(File {
                    index: index as usize,
                    path,
                    size: ht_meta_file_size(self.0, index),
                });
            }
        }
        Ok(files)
    }
    pub fn bytes(&self) -> Vec<u8> {
        unsafe {
            let len = ht_meta_size(self.0);
            if len == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(ht_meta_bytes(self.0).cast(), len).to_vec()
            }
        }
    }
    pub fn piece_length(&self) -> usize {
        unsafe { ht_meta_piece_length(self.0) as usize }
    }
    pub fn magnet(&self) -> Result<String> {
        string(unsafe { ht_meta_magnet(self.0) })
    }
}

struct FileArray {
    _paths: Vec<CString>,
    native: Vec<NativeFile>,
}
impl FileArray {
    fn new(files: &[File]) -> Result<Self> {
        let paths = files
            .iter()
            .map(|f| {
                CString::new(f.path.as_str()).map_err(|_| Error::Invalid("invalid path".into()))
            })
            .collect::<Result<Vec<_>>>()?;
        let native = paths
            .iter()
            .zip(files)
            .map(|(path, file)| NativeFile {
                path: path.as_ptr(),
                size: file.size,
            })
            .collect();
        Ok(Self {
            _paths: paths,
            native,
        })
    }
}
struct SourceBridge {
    snapshot: Snapshot,
    runtime: tokio::runtime::Handle,
}
unsafe extern "C" fn read(
    ctx: *mut c_void,
    file: u32,
    offset: u64,
    len: u32,
    out: *mut c_char,
) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: Native retains the boxed context until its workers have joined.
        let source = unsafe { &*ctx.cast::<SourceBridge>() };
        let bytes =
            source
                .runtime
                .block_on(source.snapshot.read(file as usize, offset, len as usize))?;
        if bytes.len() != len as usize {
            return Err(Error::Content);
        }
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.cast(), bytes.len());
        }
        Ok(())
    }));
    if matches!(result, Ok(Ok::<(), Error>(()))) {
        0
    } else {
        -1
    }
}
fn cancel_ptr(cancel: &AtomicBool) -> *mut c_void {
    (cancel as *const AtomicBool).cast_mut().cast()
}
unsafe extern "C" fn cancelled(ctx: *mut c_void) -> i32 {
    i32::from(unsafe { &*ctx.cast::<AtomicBool>() }.load(Ordering::Acquire))
}
pub(crate) struct CancelOnDrop(pub Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

pub(crate) struct Native {
    ptr: *mut c_void,
    _source: Option<Box<SourceBridge>>,
}
// SAFETY: sessions are moved between threads only behind an exclusive async mutex.
unsafe impl Send for Native {}
impl Drop for Native {
    fn drop(&mut self) {
        // The destructor joins disk/network workers before _source can be dropped.
        unsafe {
            ht_session_free(self.ptr);
        }
    }
}
impl Native {
    pub fn open(
        meta: Option<&RawMetadata>,
        magnet: Option<&str>,
        snapshot: Option<Snapshot>,
        options: &NetworkOptions,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self> {
        options.validate()?;
        let listen = CString::new(options.listen.to_string()).unwrap();
        let bootstrap = CString::new(
            options
                .bootstrap
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(","),
        )
        .unwrap();
        let opts = Options {
            listen: listen.as_ptr(),
            bootstrap: bootstrap.as_ptr(),
            cache_bytes: options.cache_bytes as u64,
            connections: options.connections as u32,
            upload_limit: options.upload_limit,
            download_limit: options.download_limit,
            dht: options.dht as u8,
            trackers: options.trackers as u8,
        };
        let mut source = snapshot.map(|snapshot| Box::new(SourceBridge { snapshot, runtime }));
        let files = FileArray::new(source.as_ref().map_or(&[], |s| s.snapshot.files()))?;
        let ctx = source.as_mut().map_or(std::ptr::null_mut(), |s| {
            (&mut **s as *mut SourceBridge).cast()
        });
        let ptr = if let Some(meta) = meta {
            unsafe {
                ht_session_open(
                    meta.0,
                    &opts,
                    files.native.as_ptr(),
                    files.native.len(),
                    source.as_ref().map(|_| read as ReadFn),
                    ctx,
                )
            }
        } else {
            let magnet =
                CString::new(magnet.ok_or_else(|| Error::Invalid("missing torrent".into()))?)
                    .map_err(|_| Error::Invalid("invalid magnet".into()))?;
            unsafe { ht_session_magnet(magnet.as_ptr(), &opts) }
        };
        if ptr.is_null() {
            return Err(last_error());
        }
        let mut native = Self {
            ptr,
            _source: source,
        };
        for peer in &options.peers {
            native.connect(*peer)?;
        }
        Ok(native)
    }
    pub fn connect(&mut self, peer: std::net::SocketAddr) -> Result<()> {
        if peer.port() == 0 || peer.ip().is_unspecified() || peer.ip().is_multicast() {
            return Err(Error::Invalid("invalid peer address".into()));
        }
        let address = CString::new(peer.ip().to_string()).unwrap();
        check(unsafe { ht_session_connect(self.ptr, address.as_ptr(), peer.port()) })
    }
    pub fn metadata(&mut self, timeout: u64, cancel: &AtomicBool) -> Result<RawMetadata> {
        RawMetadata::from_ptr(unsafe {
            ht_session_metadata(self.ptr, timeout, cancelled, cancel_ptr(cancel))
        })
    }
    pub fn read(
        &mut self,
        file: usize,
        offset: u64,
        len: usize,
        timeout: u64,
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>> {
        let mut bytes = vec![0; len];
        check(unsafe {
            ht_session_read(
                self.ptr,
                file as i32,
                offset,
                len as u32,
                bytes.as_mut_ptr().cast(),
                timeout,
                cancelled,
                cancel_ptr(cancel),
            )
        })?;
        Ok(bytes)
    }
    pub fn stats(&mut self) -> Result<Stats> {
        let mut stats = Stats::default();
        check(unsafe { ht_session_stats(self.ptr, &mut stats) })?;
        Ok(stats)
    }
}

pub(crate) async fn invoke<T: Send + 'static>(
    engine: Arc<tokio::sync::Mutex<Native>>,
    action: impl FnOnce(&mut Native, &AtomicBool) -> Result<T> + Send + 'static,
) -> Result<T> {
    let mut engine = engine.lock_owned().await;
    let cancel = Arc::new(AtomicBool::new(false));
    let _guard = CancelOnDrop(cancel.clone());
    tokio::task::spawn_blocking(move || action(&mut engine, &cancel)).await?
}
