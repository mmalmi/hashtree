use anyhow::{Context, Result};
use std::future::Future;
use std::sync::OnceLock;
use tokio::runtime::{Handle, Runtime, RuntimeFlavor};

static SHARED_RUNTIME: OnceLock<Runtime> = OnceLock::new();

fn new_multi_thread_runtime() -> std::io::Result<Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
}

fn shared_runtime() -> Result<&'static Runtime> {
    if let Some(rt) = SHARED_RUNTIME.get() {
        return Ok(rt);
    }
    let rt = new_multi_thread_runtime().context("Failed to create tokio runtime")?;
    Ok(SHARED_RUNTIME.get_or_init(|| rt))
}

pub(crate) fn block_on<T, F>(future: F) -> Result<T>
where
    F: Future<Output = T>,
{
    if let Ok(handle) = Handle::try_current() {
        return Ok(match handle.runtime_flavor() {
            RuntimeFlavor::MultiThread => tokio::task::block_in_place(|| handle.block_on(future)),
            RuntimeFlavor::CurrentThread => handle.block_on(future),
            _ => tokio::task::block_in_place(|| handle.block_on(future)),
        });
    }
    Ok(shared_runtime()?.block_on(future))
}

pub(crate) fn block_on_result<T, F>(future: F) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    block_on(future)?
}
