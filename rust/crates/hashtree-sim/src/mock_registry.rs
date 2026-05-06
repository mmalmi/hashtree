use tokio::sync::{Mutex, MutexGuard};

static MOCK_CHANNEL_REGISTRY_LOCK: Mutex<()> = Mutex::const_new(());

pub(crate) async fn lock_mock_channel_registry() -> MutexGuard<'static, ()> {
    MOCK_CHANNEL_REGISTRY_LOCK.lock().await
}
