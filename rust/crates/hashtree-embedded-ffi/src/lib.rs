use hashtree_embedded::{HostDaemonOptions, HostDaemonRuntime};
use std::ffi::{c_char, c_void, CString};
use std::path::PathBuf;
use std::ptr;
use std::sync::Mutex;

struct EmbeddedDaemonHandle {
    runtime: HostDaemonRuntime,
}

static LAST_ERROR: Mutex<Option<CString>> = Mutex::new(None);

fn set_last_error(message: impl Into<String>) {
    let message = sanitize_c_string(message.into());
    *LAST_ERROR.lock().expect("last error lock") = Some(message);
}

fn clear_last_error() {
    *LAST_ERROR.lock().expect("last error lock") = None;
}

fn sanitize_c_string(input: String) -> CString {
    let sanitized = input.replace('\0', " ");
    CString::new(sanitized).expect("CString conversion should succeed after sanitizing NULs")
}

unsafe fn string_from_ptr(ptr: *const c_char, label: &str) -> Result<String, String> {
    if ptr.is_null() {
        return Err(format!("{label} pointer was null"));
    }
    let c_str = unsafe { std::ffi::CStr::from_ptr(ptr) };
    c_str
        .to_str()
        .map(|value| value.to_owned())
        .map_err(|_| format!("{label} was not valid UTF-8"))
}

#[unsafe(no_mangle)]
pub extern "C" fn hashtree_embedded_start(state_root: *const c_char) -> *mut c_void {
    clear_last_error();

    let state_root = match unsafe { string_from_ptr(state_root, "state_root") } {
        Ok(value) => value,
        Err(error) => {
            set_last_error(error);
            return ptr::null_mut();
        }
    };

    let options = HostDaemonOptions::new(PathBuf::from(state_root));
    match HostDaemonRuntime::start(options) {
        Ok(runtime) => Box::into_raw(Box::new(EmbeddedDaemonHandle { runtime })) as *mut c_void,
        Err(error) => {
            set_last_error(format!(
                "failed to start embedded hashtree daemon: {error:#}"
            ));
            ptr::null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn hashtree_embedded_shutdown(handle: *mut c_void) {
    if handle.is_null() {
        return;
    }
    let mut handle = unsafe { Box::from_raw(handle.cast::<EmbeddedDaemonHandle>()) };
    handle.runtime.shutdown();
}

#[unsafe(no_mangle)]
pub extern "C" fn hashtree_embedded_reload(handle: *mut c_void) -> bool {
    clear_last_error();
    if handle.is_null() {
        set_last_error("embedded daemon handle was null");
        return false;
    }
    let handle = unsafe { &mut *handle.cast::<EmbeddedDaemonHandle>() };
    match handle.runtime.reload() {
        Ok(_) => true,
        Err(error) => {
            set_last_error(format!(
                "failed to reload embedded hashtree daemon: {error:#}"
            ));
            false
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn hashtree_embedded_get_base_url(handle: *const c_void) -> *mut c_char {
    clear_last_error();
    if handle.is_null() {
        set_last_error("embedded daemon handle was null");
        return ptr::null_mut();
    }
    let handle = unsafe { &*handle.cast::<EmbeddedDaemonHandle>() };
    sanitize_c_string(handle.runtime.base_url()).into_raw()
}

#[unsafe(no_mangle)]
pub extern "C" fn hashtree_embedded_get_self_npub(handle: *const c_void) -> *mut c_char {
    clear_last_error();
    if handle.is_null() {
        set_last_error("embedded daemon handle was null");
        return ptr::null_mut();
    }
    let handle = unsafe { &*handle.cast::<EmbeddedDaemonHandle>() };
    sanitize_c_string(handle.runtime.self_npub().to_owned()).into_raw()
}

#[unsafe(no_mangle)]
pub extern "C" fn hashtree_embedded_take_last_error() -> *mut c_char {
    let mut last_error = LAST_ERROR.lock().expect("last error lock");
    last_error
        .take()
        .map(CString::into_raw)
        .unwrap_or(ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "C" fn hashtree_embedded_string_free(value: *mut c_char) {
    if value.is_null() {
        return;
    }
    unsafe {
        drop(CString::from_raw(value));
    }
}
