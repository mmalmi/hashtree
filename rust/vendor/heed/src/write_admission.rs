use std::sync::Arc;
use std::{fmt, io};

type Callback = dyn Fn(i32, u64, usize) -> io::Result<()> + Send + Sync;

struct Inner {
    #[cfg_attr(not(unix), allow(dead_code))]
    callback: Box<Callback>,
}

/// Optional admission immediately before each physical LMDB write.
///
/// The callback receives a borrowed Unix file descriptor, byte offset and exact
/// syscall length. Commit metadata reserves twice its write length up front,
/// allowing one mandatory best-effort restore after a partial write without a
/// second admission check. It must not close/retain the descriptor or reenter LMDB. An
/// error refuses the write; its positive OS errno is preserved. Panics and
/// errors without a positive OS errno become `EIO`. Guarded opens on non-Unix
/// systems, and guarded writable mappings, fail closed. Environment-copy output
/// is outside this hook. Clone the same handle when reopening an environment:
/// callback identity participates in the existing open-options compatibility check.
#[derive(Clone)]
pub struct WriteAdmission(Arc<Inner>);

impl WriteAdmission {
    /// Create a callback retained until its LMDB environment closes.
    pub fn new<F>(callback: F) -> Self
    where
        F: Fn(i32, u64, usize) -> io::Result<()> + Send + Sync + 'static,
    {
        Self(Arc::new(Inner {
            callback: Box::new(callback),
        }))
    }

    #[cfg(unix)]
    pub(crate) fn context(&self) -> *mut std::ffi::c_void {
        Arc::as_ptr(&self.0).cast_mut().cast()
    }
}

impl PartialEq for WriteAdmission {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl fmt::Debug for WriteAdmission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WriteAdmission").finish_non_exhaustive()
    }
}

#[cfg(unix)]
pub(crate) unsafe extern "C" fn admit_write(
    context: *mut std::ffi::c_void,
    fd: crate::mdb::ffi::mdb_filehandle_t,
    offset: u64,
    length: usize,
) -> libc::c_int {
    if context.is_null() {
        return libc::EIO;
    }
    let inner = unsafe { &*context.cast::<Inner>() };
    let callback = &inner.callback;
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        callback(fd, offset, length)
    })) {
        Ok(Ok(())) => 0,
        Ok(Err(error)) => error
            .raw_os_error()
            .filter(|code| *code > 0)
            .unwrap_or(libc::EIO),
        Err(_) => libc::EIO,
    }
}

// Serializing options must never silently turn a guarded open into an unguarded one.
#[cfg(feature = "serde")]
impl serde::Serialize for WriteAdmission {
    fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
        Err(serde::ser::Error::custom(
            "write admission callbacks cannot be serialized",
        ))
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for WriteAdmission {
    fn deserialize<D: serde::Deserializer<'de>>(_: D) -> Result<Self, D::Error> {
        Err(serde::de::Error::custom(
            "write admission callbacks must be installed explicitly",
        ))
    }
}

#[cfg(all(test, unix))]
mod tests;
