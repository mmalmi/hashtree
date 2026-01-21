pub mod history;
pub mod htree;
pub mod nip07;
pub mod permissions;
pub mod relay_proxy;
pub mod worker;

use tauri::menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder};
use tauri::{Emitter, Manager};
use tracing::info;
use tracing_subscriber::EnvFilter;

fn build_menu<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> tauri::Result<tauri::menu::Menu<R>> {
    let app_name = app.package_info().name.clone();
    let quit = MenuItemBuilder::with_id("app_quit", "Quit")
        .accelerator("CmdOrCtrl+Q")
        .build(app)?;
    let app_menu = SubmenuBuilder::new(app, app_name).item(&quit).build()?;

    let back = MenuItemBuilder::with_id("nav_back", "Back")
        .accelerator("CmdOrCtrl+Left")
        .build(app)?;
    let forward = MenuItemBuilder::with_id("nav_forward", "Forward")
        .accelerator("CmdOrCtrl+Right")
        .build(app)?;

    let navigation = SubmenuBuilder::new(app, "Navigation")
        .item(&back)
        .item(&forward)
        .build()?;

    MenuBuilder::new(app).item(&app_menu).item(&navigation).build()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Initialize tracing with env filter (RUST_LOG=iris=debug)
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("iris=info")),
        )
        .init();

    tauri::Builder::default()
        .menu(build_menu)
        .on_menu_event(|app, event| {
            match event.id().as_ref() {
                "nav_back" => {
                    let _ = app.emit(
                        "child-webview-navigate",
                        serde_json::json!({
                            "action": "back"
                        }),
                    );
                }
                "nav_forward" => {
                    let _ = app.emit(
                        "child-webview-navigate",
                        serde_json::json!({
                            "action": "forward"
                        }),
                    );
                }
                "app_quit" => {
                    app.exit(0);
                }
                _ => {}
            }
        })
        .plugin(tauri_plugin_os::init())
        .register_uri_scheme_protocol("htree", htree::handle_htree_protocol)
        .invoke_handler(tauri::generate_handler![
            htree::get_htree_server_url,
            htree::cache_tree_root,
            htree::webview_event,
            worker::worker_message,
            nip07::create_nip07_webview,
            nip07::navigate_webview,
            nip07::webview_history,
            nip07::webview_current_url,
            nip07::nip07_request,
            history::record_history_visit,
            history::search_history,
            history::get_recent_history
        ])
        .on_page_load(|webview, payload| {
            // Inject NIP-07 window.nostr on page load for main window
            if webview.label() == "main" {
                if matches!(payload.event(), tauri::webview::PageLoadEvent::Finished) {
                    info!("Main window page loaded: {}", payload.url());
                    let script = nip07::generate_main_window_nip07_script();
                    if let Err(e) = webview.eval(&script) {
                        tracing::warn!("Failed to inject NIP-07 script: {}", e);
                    } else {
                        info!("Injected NIP-07 window.nostr into main window");
                    }
                }
            }
        })
        .setup(|app| {
            let data_dir = app
                .path()
                .app_data_dir()
                .expect("failed to get app data dir");
            std::fs::create_dir_all(&data_dir).expect("failed to create data dir");

            info!("App data directory: {:?}", data_dir);

            // Initialize htree state for URI scheme protocol (must be before webview loads)
            htree::init_htree_state(data_dir.clone());
            info!("htree:// protocol initialized");
            htree::set_app_handle(app.handle().clone());

            // Initialize worker state (store + tree manager + nostrdb)
            let blob_store = worker::BlobStore::new(data_dir.clone());
            let worker_state = std::sync::Arc::new(
                worker::WorkerState::new(blob_store, data_dir.clone())
                    .expect("failed to initialize worker state"),
            );

            // Initialize NIP-07 state for permission management
            let permission_store = std::sync::Arc::new(permissions::PermissionStore::new(None));
            let nip07_state = std::sync::Arc::new(nip07::Nip07State::new(permission_store));

            // Initialize history store for search suggestions
            let history_store = std::sync::Arc::new(
                history::HistoryStore::new(&data_dir)
                    .expect("failed to initialize history store"),
            );

            // Initialize global state for HTTP handler access (must be before manage)
            nip07::init_global_state(nip07_state.clone(), worker_state.clone());

            // Manage Arc-wrapped states for Tauri
            app.manage(worker_state);
            app.manage(nip07_state);
            app.manage(history_store);

            // Start the htree HTTP server with access to local blob store
            let htree_data_dir = data_dir.clone();
            tauri::async_runtime::spawn(async move {
                match htree::start_server(htree_data_dir).await {
                    Ok(port) => {
                        info!("htree server started on port {}", port);
                    }
                    Err(e) => {
                        tracing::error!("Failed to start htree server: {}", e);
                    }
                }
            });

            // Auto-initialize Nostr client and WebRTC on startup (for headless/test mode)
            // This ensures the backend is ready even if frontend takes time to load
            let state_handle: tauri::State<'_, std::sync::Arc<worker::WorkerState>> = app.state();
            let nostr = state_handle.nostr.clone();
            let webrtc = state_handle.webrtc.clone();
            let ndb = state_handle.ndb.clone();
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                // Initialize Nostr client
                if let Err(e) = nostr.ensure_client(Some(app_handle), Some(ndb)).await {
                    tracing::warn!("Failed to auto-initialize Nostr client: {}", e);
                    return;
                }

                // Initialize WebRTC with the Nostr client
                // Use identity keys if set, otherwise use ephemeral keys from init
                if let Some(client) = nostr.get_client() {
                    let keys = nostr.get_keys().unwrap_or_else(nostr_sdk::Keys::generate);
                    if let Err(e) = webrtc.init(client, keys).await {
                        tracing::warn!("Failed to auto-initialize WebRTC: {}", e);
                    }
                }
            });

            // Check if launched with --minimized flag (from autostart) - desktop only
            #[cfg(any(target_os = "macos", windows, target_os = "linux"))]
            {
                let args: Vec<String> = std::env::args().collect();
                if args.contains(&"--minimized".to_string()) {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.minimize();
                        info!("Started minimized (autostart)");
                    }
                }
            }

            // Add notification plugin
            app.handle().plugin(tauri_plugin_notification::init())?;

            // Add opener plugin for external links
            app.handle().plugin(tauri_plugin_opener::init())?;

            // Add dialog plugin for file operations
            app.handle().plugin(tauri_plugin_dialog::init())?;

            // Add autostart plugin for desktop platforms
            #[cfg(any(target_os = "macos", windows, target_os = "linux"))]
            app.handle().plugin(tauri_plugin_autostart::init(
                tauri_plugin_autostart::MacosLauncher::LaunchAgent,
                Some(vec!["--minimized"]),
            ))?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::build_menu;

    #[test]
    fn app_menu_includes_quit_item() {
        let app = tauri::test::mock_app();
        let handle = app.handle();
        let menu = build_menu(&handle).expect("failed to build menu");
        let mut has_quit = false;

        for item in menu.items().unwrap_or_default() {
            if let tauri::menu::MenuItemKind::Submenu(submenu) = item {
                for subitem in submenu.items().unwrap_or_default() {
                    if subitem.id().as_ref() == "app_quit" {
                        has_quit = true;
                    }
                }
            }
        }

        assert!(has_quit, "expected app_quit menu item");
    }
}
