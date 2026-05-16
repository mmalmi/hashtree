use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use tauri::{ipc::Channel, AppHandle, Runtime};

use crate::error::Result;
use crate::updater::{context_from_app, InstallOverrides};
use hashtree_updater::DownloadEvent as CoreDownloadEvent;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", content = "data", rename_all = "camelCase")]
pub enum DownloadEvent {
    #[serde(rename_all = "camelCase")]
    Started { content_length: Option<u64> },
    #[serde(rename_all = "camelCase")]
    Progress { chunk_length: u64, downloaded: u64 },
    #[serde(rename_all = "camelCase")]
    Finished { total: u64 },
}

impl From<CoreDownloadEvent> for DownloadEvent {
    fn from(value: CoreDownloadEvent) -> Self {
        match value {
            CoreDownloadEvent::Started { content_length } => Self::Started { content_length },
            CoreDownloadEvent::Progress {
                chunk_len,
                downloaded,
            } => Self::Progress {
                chunk_length: chunk_len,
                downloaded,
            },
            CoreDownloadEvent::Finished { total } => Self::Finished { total },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateMetadata {
    pub current_version: String,
    pub version: String,
    pub asset_name: String,
    pub asset_kind: String,
    pub notes: Option<String>,
    pub published_at: Option<String>,
    pub update_available: bool,
}

impl From<crate::updater::CheckedUpdate> for UpdateMetadata {
    fn from(value: crate::updater::CheckedUpdate) -> Self {
        Self {
            current_version: value.current_version,
            version: value.version,
            asset_name: value.asset_name,
            asset_kind: value.asset_kind,
            notes: value.notes,
            published_at: value.published_at,
            update_available: value.update_available,
        }
    }
}

#[tauri::command]
pub(crate) async fn check<R: Runtime>(app: AppHandle<R>) -> Result<Option<UpdateMetadata>> {
    let ctx = context_from_app(&app);
    Ok(ctx.check().await?.map(UpdateMetadata::from))
}

#[tauri::command]
pub(crate) async fn download_and_install<R: Runtime>(
    app: AppHandle<R>,
    on_event: Channel<DownloadEvent>,
    destination: Option<PathBuf>,
    kind: Option<String>,
    executable: Option<bool>,
) -> Result<UpdateMetadata> {
    let ctx = context_from_app(&app);
    let on_event = Arc::new(move |event: CoreDownloadEvent| {
        let _ = on_event.send(DownloadEvent::from(event));
    });
    let overrides = InstallOverrides {
        destination,
        kind,
        executable: executable.unwrap_or(false),
    };
    let result = ctx.download_and_install(overrides, Some(on_event)).await?;
    Ok(UpdateMetadata::from(result))
}
