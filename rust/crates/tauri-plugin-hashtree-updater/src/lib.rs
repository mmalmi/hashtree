//! Tauri v2 plugin for hashtree-based in-app updates.
//!
//! Configure under `plugins.hashtree-updater` in `tauri.conf.json`:
//!
//! ```json
//! {
//!   "plugins": {
//!     "hashtree-updater": {
//!       "reference": "htree://npub1.../releases%2Fmyapp/stable/latest",
//!       "destination": "/Applications/MyApp.app",
//!       "relays": ["wss://relay.iris.to"]
//!     }
//!   }
//! }
//! ```
//!
//! Frontend usage (TypeScript):
//!
//! ```ts
//! import { check } from '@hashtree/tauri-plugin-updater';
//! const update = await check();
//! if (update?.updateAvailable) {
//!   await update.downloadAndInstall((event) => console.log(event));
//! }
//! ```

mod commands;
mod config;
mod error;
mod updater;

pub use config::Config;
pub use error::{Error, Result};
pub use updater::{CheckedUpdate, InstallOverrides, UpdaterContext};

use hashtree_updater::NostrEventSubscriber;
use std::sync::Arc;

use tauri::{
    plugin::{Builder as PluginBuilder, TauriPlugin},
    Manager, Runtime,
};

pub(crate) struct PluginState {
    pub(crate) config: Config,
    pub(crate) provider: Option<Arc<dyn NostrEventSubscriber>>,
    pub(crate) resolver: Arc<tokio::sync::OnceCell<hashtree_updater::PubsubRootResolver>>,
}

/// Convenience accessor for the plugin's `UpdaterContext` from a Tauri app
/// handle. Useful for invoking checks from Rust without going through IPC.
pub trait HashtreeUpdaterExt<R: Runtime> {
    fn hashtree_updater(&self) -> UpdaterContext;
}

impl<R: Runtime, T: Manager<R>> HashtreeUpdaterExt<R> for T {
    fn hashtree_updater(&self) -> UpdaterContext {
        updater::context_from_app(self.app_handle())
    }
}

pub fn init<R: Runtime>() -> TauriPlugin<R, Config> {
    build_plugin(None)
}

/// Share the application's pubsub transport and relay policy with the updater.
pub fn init_with_provider<R: Runtime>(
    provider: Arc<dyn NostrEventSubscriber>,
) -> TauriPlugin<R, Config> {
    build_plugin(Some(provider))
}

fn build_plugin<R: Runtime>(
    provider: Option<Arc<dyn NostrEventSubscriber>>,
) -> TauriPlugin<R, Config> {
    PluginBuilder::<R, Config>::new("hashtree-updater")
        .setup(move |app, api| {
            let config = api.config().clone();
            app.manage(PluginState {
                config,
                provider,
                resolver: Arc::default(),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::check,
            commands::download_and_install,
        ])
        .build()
}
