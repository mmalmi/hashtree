//! NIP-07 webview support
//!
//! Provides window.nostr capability for child webviews by:
//! 1. Injecting initialization script that defines window.nostr
//! 2. Using HTTP calls to localhost with session token for security
//! 3. Handling NIP-07 requests via the htree HTTP server

use crate::permissions::{PermissionStore, PermissionType};
use crate::worker::WorkerState;
use nostr_sdk::{Kind, Tag, Timestamp, UnsignedEvent};
use once_cell::sync::OnceCell;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, Runtime, WebviewBuilder, WebviewUrl};
use tracing::{debug, info};

// ============================================
// htree:// URL helpers for origin isolation
// ============================================

/// Construct htree:// origin from nhash (for storage isolation)
/// Example: "nhash1abc123" → "htree://nhash1abc123"
pub fn htree_origin_from_nhash(nhash: &str) -> String {
    format!("htree://{}", nhash)
}

/// Construct htree:// origin from npub and treename (for storage isolation)
/// Uses dot separator since "/" isn't valid in hostname
/// Example: ("npub1xyz", "public") → "htree://npub1xyz.public"
pub fn htree_origin_from_npub(npub: &str, treename: &str) -> String {
    format!("htree://{}.{}", npub, treename)
}

/// Construct htree:// URL from nhash with optional path
/// Example: ("nhash1abc", "index.html") → "htree://nhash1abc/index.html"
pub fn htree_url_from_nhash(nhash: &str, path: &str) -> String {
    if path.is_empty() || path == "/" {
        format!("htree://{}", nhash)
    } else {
        let path = path.trim_start_matches('/');
        format!("htree://{}/{}", nhash, path)
    }
}

/// Construct htree:// URL from npub/treename with optional path
/// Example: ("npub1xyz", "public", "index.html") → "htree://npub1xyz.public/index.html"
pub fn htree_url_from_npub(npub: &str, treename: &str, path: &str) -> String {
    if path.is_empty() || path == "/" {
        format!("htree://{}.{}", npub, treename)
    } else {
        let path = path.trim_start_matches('/');
        format!("htree://{}.{}/{}", npub, treename, path)
    }
}

/// Parse htree:// host to extract nhash or npub/treename
/// Returns (nhash, npub, treename) where only one of nhash/npub will be Some
pub fn parse_htree_host(host: &str) -> Option<(Option<String>, Option<String>, Option<String>)> {
    if host.starts_with("nhash1") {
        // nhash host: htree://nhash1abc.../path
        Some((Some(host.to_string()), None, None))
    } else if host.starts_with("npub1") {
        // npub host: htree://npub1xyz.treename/path
        // npub is always 63 chars (npub1 + 58 bech32 chars)
        if host.len() >= 63 {
            let npub = &host[..63];
            let rest = &host[63..];
            if rest.is_empty() {
                // No treename separator
                Some((None, Some(npub.to_string()), None))
            } else if rest.starts_with('.') && rest.len() > 1 {
                // Has treename: npub1xyz.treename
                let treename = &rest[1..];
                Some((None, Some(npub.to_string()), Some(treename.to_string())))
            } else {
                // Invalid format
                None
            }
        } else {
            None
        }
    } else {
        None
    }
}

/// Global state for NIP-07 HTTP handler access
static GLOBAL_NIP07_STATE: OnceCell<Arc<Nip07State>> = OnceCell::new();
static GLOBAL_WORKER_STATE: OnceCell<Arc<WorkerState>> = OnceCell::new();

/// Initialize global state for HTTP handler access
pub fn init_global_state(nip07: Arc<Nip07State>, worker: Arc<WorkerState>) {
    let _ = GLOBAL_NIP07_STATE.set(nip07);
    let _ = GLOBAL_WORKER_STATE.set(worker);
}

/// Get global NIP-07 state
pub fn get_nip07_state() -> Option<Arc<Nip07State>> {
    GLOBAL_NIP07_STATE.get().cloned()
}

/// Get global worker state
pub fn get_worker_state() -> Option<Arc<WorkerState>> {
    GLOBAL_WORKER_STATE.get().cloned()
}

/// Generate NIP-07 initialization script for main window using Tauri invoke
pub fn generate_main_window_nip07_script() -> String {
    r#"
(function() {
  if (window.nostr) {
    console.log('[NIP-07] Already initialized');
    return;
  }

  console.log('[NIP-07] Initializing for main window via Tauri invoke');

  // Helper to get invoke function, waiting for Tauri if needed
  async function getInvoke() {
    // Check various locations where invoke might be
    if (window.__TAURI_INTERNALS__?.invoke) return window.__TAURI_INTERNALS__.invoke;
    if (window.__TAURI__?.core?.invoke) return window.__TAURI__.core.invoke;
    if (window.__TAURI__?.invoke) return window.__TAURI__.invoke;

    // Wait for Tauri to be available (max 5 seconds)
    for (let i = 0; i < 50; i++) {
      await new Promise(r => setTimeout(r, 100));
      if (window.__TAURI_INTERNALS__?.invoke) return window.__TAURI_INTERNALS__.invoke;
      if (window.__TAURI__?.core?.invoke) return window.__TAURI__.core.invoke;
      if (window.__TAURI__?.invoke) return window.__TAURI__.invoke;
    }
    throw new Error('Tauri invoke not available after timeout');
  }

  async function callNip07(method, params) {
    console.log('[NIP-07] Calling:', method, params);
    try {
      const invoke = await getInvoke();
      const result = await invoke('nip07_request', {
        method,
        params: params || {},
        origin: 'tauri://localhost'
      });
      console.log('[NIP-07] Result:', result);
      if (result.error) {
        throw new Error(result.error);
      }
      return result.result;
    } catch (e) {
      console.error('[NIP-07] Error:', e);
      throw e;
    }
  }

  window.nostr = {
    async getPublicKey() {
      return callNip07('getPublicKey', {});
    },

    async signEvent(event) {
      return callNip07('signEvent', { event });
    },

    async getRelays() {
      return callNip07('getRelays', {});
    },

    nip04: {
      async encrypt(pubkey, plaintext) {
        return callNip07('nip04.encrypt', { pubkey, plaintext });
      },
      async decrypt(pubkey, ciphertext) {
        return callNip07('nip04.decrypt', { pubkey, ciphertext });
      }
    },

    nip44: {
      async encrypt(pubkey, plaintext) {
        return callNip07('nip44.encrypt', { pubkey, plaintext });
      },
      async decrypt(pubkey, ciphertext) {
        return callNip07('nip44.decrypt', { pubkey, ciphertext });
      }
    }
  };

  console.log('[NIP-07] window.nostr initialized for main window');
})();
"#.to_string()
}

/// Generate NIP-07 initialization script with server URL and session token
pub fn generate_nip07_script(server_url: &str, session_token: &str, label: &str) -> String {
    format!(
        r#"
(function() {{
  const hasNostr = !!window.nostr;
  const SERVER_URL = "{}";
  const SESSION_TOKEN = "{}";
  const WEBVIEW_LABEL = "{}";
  const NAV_ENDPOINT = `${{SERVER_URL}}/webview`;
  console.log('[NIP-07] Initializing with server:', SERVER_URL);
  window.__HTREE_SERVER_URL__ = SERVER_URL;

  let invokePromise = null;
  async function getInvoke() {{
    if (invokePromise) return invokePromise;
    invokePromise = (async () => {{
      const getNow = () =>
        window.__TAURI_INTERNALS__?.invoke ||
        window.__TAURI__?.core?.invoke ||
        window.__TAURI__?.invoke ||
        null;
      const immediate = getNow();
      if (immediate) return immediate;
      for (let i = 0; i < 20; i++) {{
        await new Promise((resolve) => setTimeout(resolve, 50));
        const candidate = getNow();
        if (candidate) return candidate;
      }}
      return null;
    }})();
    return invokePromise;
  }}

  function getOrigin() {{
    const origin = window.location.origin;
    if (origin && origin !== 'null') return origin;
    const protocol = window.location.protocol || '';
    const normalizedProtocol = protocol.endsWith(':') ? protocol.slice(0, -1) : protocol;
    const host = window.location.host || '';
    if (host) return `${{normalizedProtocol}}://${{host}}`;
    return normalizedProtocol || 'null';
  }}

  async function postWebviewEvent(payload) {{
    try {{
      const invoke = await getInvoke();
      if (invoke) {{
        await invoke('webview_event', {{
          payload,
          session_token: SESSION_TOKEN
        }});
        return;
      }}
    }} catch (error) {{
      console.warn('[WebviewBridge] Failed to send event via invoke', error);
    }}
    fetch(NAV_ENDPOINT, {{
      method: 'POST',
      headers: {{
        'Content-Type': 'application/json',
        'X-Session-Token': SESSION_TOKEN
      }},
      body: JSON.stringify(payload)
    }}).catch((error) => {{
      console.warn('[WebviewBridge] Failed to send event', error);
    }});
  }}

  let lastLocation = null;
  function notifyLocation(source) {{
    const url = window.location.href;
    if (url === lastLocation) return;
    lastLocation = url;
    postWebviewEvent({{
      kind: 'location',
      label: WEBVIEW_LABEL,
      origin: getOrigin(),
      url,
      source
    }});
  }}

  const originalPushState = history.pushState;
  history.pushState = function(state, title, url) {{
    const result = originalPushState.apply(this, arguments);
    notifyLocation('pushState');
    return result;
  }};

  const originalReplaceState = history.replaceState;
  history.replaceState = function(state, title, url) {{
    const result = originalReplaceState.apply(this, arguments);
    notifyLocation('replaceState');
    return result;
  }};

  const historyProto = Object.getPrototypeOf(history) || (typeof History !== 'undefined' ? History.prototype : null);
  const originalBack = historyProto && typeof historyProto.back === 'function' ? historyProto.back : null;
  const originalForward = historyProto && typeof historyProto.forward === 'function' ? historyProto.forward : null;

  function notifyNavigate(action) {{
    postWebviewEvent({{
      kind: 'navigate',
      label: WEBVIEW_LABEL,
      origin: getOrigin(),
      action
    }});
  }}

  function scheduleHistoryFallback(action, beforeUrl) {{
    setTimeout(() => {{
      if (window.location.href !== beforeUrl) return;
      notifyNavigate(action);
      if (action === 'back') {{
        window.location.href = 'about:blank';
      }}
    }}, 300);
  }}

  function wrapHistory(action, original) {{
    return function() {{
      const beforeUrl = window.location.href;
      const result = original.apply(this, arguments);
      scheduleHistoryFallback(action, beforeUrl);
      return result;
    }};
  }}

  function setHistoryMethod(target, name, value) {{
    if (!target) return false;
    try {{
      target[name] = value;
      return true;
    }} catch {{}}
    try {{
      Object.defineProperty(target, name, {{
        value,
        configurable: true,
        writable: true
      }});
      return true;
    }} catch {{}}
    return false;
  }}

  if (originalBack) {{
    const wrappedBack = wrapHistory('back', originalBack);
    setHistoryMethod(history, 'back', wrappedBack);
    setHistoryMethod(historyProto, 'back', wrappedBack);
  }}

  if (originalForward) {{
    const wrappedForward = wrapHistory('forward', originalForward);
    setHistoryMethod(history, 'forward', wrappedForward);
    setHistoryMethod(historyProto, 'forward', wrappedForward);
  }}

  window.addEventListener('popstate', () => notifyLocation('popstate'));
  window.addEventListener('hashchange', () => notifyLocation('hashchange'));
  window.addEventListener('DOMContentLoaded', () => notifyLocation('domcontentloaded'));
  window.addEventListener('load', () => notifyLocation('load'));
  queueMicrotask(() => notifyLocation('init'));

  function navigateHistory(action) {{
    const beforeUrl = window.location.href;
    if (action === 'back') {{
      if (originalBack) {{
        originalBack.call(history);
      }} else {{
        history.back();
      }}
    }} else if (action === 'forward') {{
      if (originalForward) {{
        originalForward.call(history);
      }} else {{
        history.forward();
      }}
    }}
    scheduleHistoryFallback(action, beforeUrl);
  }}

  function handleMouseUp(e) {{
    if (e.button === 3) {{
      e.preventDefault();
      navigateHistory('back');
    }} else if (e.button === 4) {{
      e.preventDefault();
      navigateHistory('forward');
    }}
  }}

  function isEditableTarget(target) {{
    if (!target) return false;
    if (target.closest && target.closest('input, textarea, select')) return true;
    return !!target.isContentEditable;
  }}

  function isEditableEventTarget(event) {{
    if (isEditableTarget(event.target)) return true;
    if (typeof event.composedPath === 'function') {{
      const path = event.composedPath();
      for (const entry of path) {{
        if (isEditableTarget(entry)) return true;
      }}
    }}
    return false;
  }}

  function handleKeyDown(e) {{
    if (isEditableEventTarget(e)) return;

    const isMac = navigator.platform && navigator.platform.toUpperCase().includes('MAC');
    const modifier = isMac ? e.metaKey : e.ctrlKey;

    if (e.key === 'BrowserBack') {{
      e.preventDefault();
      navigateHistory('back');
      return;
    }}
    if (e.key === 'BrowserForward') {{
      e.preventDefault();
      navigateHistory('forward');
      return;
    }}

    if (modifier && !e.shiftKey && !e.altKey) {{
      if (e.key === 'ArrowLeft') {{
        e.preventDefault();
        navigateHistory('back');
      }} else if (e.key === 'ArrowRight') {{
        e.preventDefault();
        navigateHistory('forward');
      }}
    }}
  }}

  const captureOptions = {{ capture: true }};
  window.addEventListener('mouseup', handleMouseUp, captureOptions);
  document.addEventListener('mouseup', handleMouseUp, captureOptions);
  window.addEventListener('keydown', handleKeyDown, captureOptions);
  document.addEventListener('keydown', handleKeyDown, captureOptions);

  async function callNip07(method, params) {{
    console.log('[NIP-07] Calling:', method, params);
    try {{
      const response = await fetch(`${{SERVER_URL}}/nip07`, {{
        method: 'POST',
        headers: {{
          'Content-Type': 'application/json',
          'X-Session-Token': SESSION_TOKEN
        }},
        body: JSON.stringify({{
          method,
          params,
          origin: getOrigin()
        }})
      }});

      console.log('[NIP-07] Response status:', response.status);
      if (!response.ok) {{
        throw new Error(`NIP-07 request failed: ${{response.status}}`);
      }}

      const result = await response.json();
      console.log('[NIP-07] Result:', result);
      if (result.error) {{
        throw new Error(result.error);
      }}
      return result.result;
    }} catch (e) {{
      console.error('[NIP-07] Error:', e);
      throw e;
    }}
  }}

  if (!hasNostr) {{
    window.nostr = {{
      async getPublicKey() {{
        return callNip07('getPublicKey', {{}});
      }},

      async signEvent(event) {{
        return callNip07('signEvent', {{ event }});
      }},

      async getRelays() {{
        return callNip07('getRelays', {{}});
      }},

      nip04: {{
        async encrypt(pubkey, plaintext) {{
          return callNip07('nip04.encrypt', {{ pubkey, plaintext }});
        }},
        async decrypt(pubkey, ciphertext) {{
          return callNip07('nip04.decrypt', {{ pubkey, ciphertext }});
        }}
      }},

      nip44: {{
        async encrypt(pubkey, plaintext) {{
          return callNip07('nip44.encrypt', {{ pubkey, plaintext }});
        }},
        async decrypt(pubkey, ciphertext) {{
          return callNip07('nip44.decrypt', {{ pubkey, ciphertext }});
        }}
      }}
    }};

    console.log('[NIP-07] window.nostr initialized');
  }} else {{
    console.log('[NIP-07] window.nostr already available');
  }}
}})();
"#,
        server_url, session_token, label
    )
}

/// State for managing NIP-07 webviews
pub struct Nip07State {
    pub permissions: Arc<PermissionStore>,
    /// Map of origin -> session token (each origin gets its own token)
    session_tokens: RwLock<HashMap<String, String>>,
}

impl Nip07State {
    pub fn new(permissions: Arc<PermissionStore>) -> Self {
        Self {
            permissions,
            session_tokens: RwLock::new(HashMap::new()),
        }
    }

    /// Generate a new session token for an origin
    pub fn new_session(&self, origin: &str) -> String {
        let token = uuid::Uuid::new_v4().to_string();
        self.session_tokens
            .write()
            .insert(origin.to_string(), token.clone());
        token
    }

    /// Validate a session token for an origin
    pub fn validate_token(&self, origin: &str, token: &str) -> bool {
        self.session_tokens
            .read()
            .get(origin)
            .map(|t| t == token)
            .unwrap_or(false)
    }

    /// Validate a session token without requiring a specific origin.
    pub fn validate_any_token(&self, token: &str) -> bool {
        self.session_tokens
            .read()
            .values()
            .any(|stored| stored == token)
    }

    /// Clear the session token for an origin
    pub fn clear_session(&self, origin: &str) {
        self.session_tokens.write().remove(origin);
    }
}

#[derive(Debug, Deserialize)]
pub struct Nip07Request {
    pub method: String,
    pub params: serde_json::Value,
    pub origin: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct Nip07Response {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Create a child webview with NIP-07 support
#[tauri::command]
pub async fn create_nip07_webview<R: Runtime>(
    app: AppHandle<R>,
    label: String,
    url: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> Result<(), String> {
    info!("[NIP-07] Creating webview {} for {}", label, url);

    // Get htree server URL
    let server_url = crate::htree::get_htree_server_url()
        .ok_or("htree server not running")?;

    // Parse origin from URL
    let parsed_url = tauri::Url::parse(&url).map_err(|e| format!("Invalid URL: {}", e))?;
    let origin = if let Some(host) = parsed_url.host_str() {
        if let Some(port) = parsed_url.port() {
            format!("{}://{}:{}", parsed_url.scheme(), host, port)
        } else {
            format!("{}://{}", parsed_url.scheme(), host)
        }
    } else {
        parsed_url.scheme().to_string()
    };

    // Generate session token for this origin
    let nip07_state = app
        .try_state::<Arc<Nip07State>>()
        .ok_or("Nip07State not found")?;
    let session_token = nip07_state.new_session(&origin);

    // Generate the initialization script with server URL and token
    let init_script = generate_nip07_script(&server_url, &session_token, &label);

    let window = app.get_window("main").ok_or("Main window not found")?;

    // Use App URL for local assets, External for remote URLs
    let mut navigate_after_create: Option<tauri::Url> = None;
    let webview_url = if url.starts_with("tauri://localhost/") {
        let mut path = parsed_url.path().trim_start_matches('/').to_string();
        if path.is_empty() {
            path = "index.html".to_string();
        }
        if parsed_url.fragment().is_some() || parsed_url.query().is_some() {
            navigate_after_create = Some(parsed_url.clone());
        }
        WebviewUrl::App(path.into())
    } else {
        WebviewUrl::External(parsed_url.clone())
    };

    // Clone app handle for the navigation callback
    let app_for_nav = app.clone();
    let label_for_nav = label.clone();

    // Create child webview with NIP-07 initialization script and navigation handler
    let webview_builder = WebviewBuilder::new(&label, webview_url)
        .initialization_script(&init_script)
        .auto_resize()
        .on_navigation(move |nav_url| {
            // Emit navigation event to the main window so it can update the URL bar
            let url_str = nav_url.to_string();
            debug!("[NIP-07] Child webview navigating to: {}", url_str);
            let _ = app_for_nav.emit(
                "child-webview-location",
                serde_json::json!({
                    "label": label_for_nav,
                    "url": url_str,
                    "source": "navigation"
                }),
            );
            // Allow the navigation
            true
        });

    let webview = window
        .add_child(
            webview_builder,
            tauri::LogicalPosition::new(x, y),
            tauri::LogicalSize::new(width, height),
        )
        .map_err(|e| format!("Failed to create webview: {}", e))?;

    if let Some(target_url) = navigate_after_create {
        if let Err(e) = webview.navigate(target_url) {
            tracing::warn!("[NIP-07] Failed to set initial URL: {}", e);
        }
    }

    info!("[NIP-07] Webview created with session token for {}", origin);

    Ok(())
}

/// Create a child webview for htree:// content with origin isolation
///
/// Each nhash or npub/treename gets its own origin for storage isolation (localStorage, IndexedDB).
/// The URL format is:
///   - htree://nhash1abc.../path (origin: htree://nhash1abc...)
///   - htree://npub1xyz.treename/path (origin: htree://npub1xyz.treename)
#[tauri::command]
pub async fn create_htree_webview<R: Runtime>(
    app: AppHandle<R>,
    label: String,
    nhash: Option<String>,
    npub: Option<String>,
    treename: Option<String>,
    path: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> Result<(), String> {
    // Validate input: either nhash or (npub + treename) must be provided
    let (url, origin) = if let Some(nhash) = &nhash {
        let url = htree_url_from_nhash(nhash, &path);
        let origin = htree_origin_from_nhash(nhash);
        (url, origin)
    } else if let (Some(npub), Some(treename)) = (&npub, &treename) {
        let url = htree_url_from_npub(npub, treename, &path);
        let origin = htree_origin_from_npub(npub, treename);
        (url, origin)
    } else {
        return Err("Either nhash or (npub + treename) must be provided".to_string());
    };

    info!(
        "[htree] Creating webview {} for {} (origin: {})",
        label, url, origin
    );

    // Get htree server URL (for NIP-07 HTTP fallback)
    let server_url = crate::htree::get_htree_server_url().ok_or("htree server not running")?;

    // Generate session token for this htree:// origin
    let nip07_state = app
        .try_state::<Arc<Nip07State>>()
        .ok_or("Nip07State not found")?;
    let session_token = nip07_state.new_session(&origin);

    // Generate the initialization script with server URL and token
    let init_script = generate_nip07_script(&server_url, &session_token, &label);

    let window = app.get_window("main").ok_or("Main window not found")?;

    // Parse the htree:// URL
    let parsed_url = tauri::Url::parse(&url).map_err(|e| format!("Invalid URL: {}", e))?;

    // Clone for navigation callback
    let app_for_nav = app.clone();
    let label_for_nav = label.clone();

    // Create child webview with htree:// URL
    let webview_builder = WebviewBuilder::new(&label, WebviewUrl::External(parsed_url))
        .initialization_script(&init_script)
        .auto_resize()
        .on_navigation(move |nav_url| {
            let url_str = nav_url.to_string();
            debug!("[htree] Child webview navigating to: {}", url_str);
            let _ = app_for_nav.emit(
                "child-webview-location",
                serde_json::json!({
                    "label": label_for_nav,
                    "url": url_str,
                    "source": "navigation"
                }),
            );
            true
        });

    window
        .add_child(
            webview_builder,
            tauri::LogicalPosition::new(x, y),
            tauri::LogicalSize::new(width, height),
        )
        .map_err(|e| format!("Failed to create webview: {}", e))?;

    info!(
        "[htree] Webview created with session token for origin {}",
        origin
    );

    Ok(())
}

/// Handle NIP-07 request (can be called from HTTP handler or Tauri command)
pub async fn handle_nip07_request(
    worker_state: &WorkerState,
    permissions: Option<&PermissionStore>,
    method: &str,
    params: &serde_json::Value,
    origin: &str,
) -> Nip07Response {
    debug!("[NIP-07] Request: {} from {}", method, origin);

    match method {
        "getPublicKey" => {
            if let Some(perms) = permissions {
                if !perms
                    .is_granted(origin, &PermissionType::GetPublicKey)
                    .await
                    .unwrap_or(true)
                {
                    return Nip07Response {
                        result: None,
                        error: Some("Permission denied".to_string()),
                    };
                }
            }

            match worker_state.nostr.get_pubkey() {
                Some(pubkey) => Nip07Response {
                    result: Some(serde_json::json!(pubkey)),
                    error: None,
                },
                None => Nip07Response {
                    result: None,
                    error: Some("No identity set".to_string()),
                },
            }
        }

        "signEvent" => {
            if let Some(perms) = permissions {
                if !perms
                    .is_granted(origin, &PermissionType::SignEvent)
                    .await
                    .unwrap_or(false)
                {
                    return Nip07Response {
                        result: None,
                        error: Some("Permission denied".to_string()),
                    };
                }
            }

            let event_value = match params.get("event") {
                Some(v) => v,
                None => {
                    return Nip07Response {
                        result: None,
                        error: Some("Missing event parameter".to_string()),
                    }
                }
            };

            let keys = match worker_state.nostr.get_keys() {
                Some(k) => k,
                None => {
                    return Nip07Response {
                        result: None,
                        error: Some("No signing keys available".to_string()),
                    }
                }
            };

            let kind = match event_value.get("kind").and_then(|v| v.as_u64()) {
                Some(k) => k as u16,
                None => {
                    return Nip07Response {
                        result: None,
                        error: Some("Missing kind".to_string()),
                    }
                }
            };
            let content = event_value
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let created_at = event_value
                .get("created_at")
                .and_then(|v| v.as_u64())
                .map(Timestamp::from)
                .unwrap_or_else(Timestamp::now);

            let tags: Vec<Tag> = event_value
                .get("tags")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|t| {
                            t.as_array().map(|tag_arr| {
                                let parts: Vec<String> = tag_arr
                                    .iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect();
                                Tag::parse(&parts).ok()
                            })
                        })
                        .flatten()
                        .collect()
                })
                .unwrap_or_default();

            let unsigned = UnsignedEvent::new(
                keys.public_key(),
                created_at,
                Kind::from(kind),
                tags,
                content,
            );

            match unsigned.sign(&keys) {
                Ok(signed_event) => match serde_json::to_value(&signed_event) {
                    Ok(event_json) => Nip07Response {
                        result: Some(event_json),
                        error: None,
                    },
                    Err(e) => Nip07Response {
                        result: None,
                        error: Some(format!("Failed to serialize event: {}", e)),
                    },
                },
                Err(e) => Nip07Response {
                    result: None,
                    error: Some(format!("Failed to sign event: {}", e)),
                },
            }
        }

        "getRelays" => Nip07Response {
            result: Some(serde_json::json!({})),
            error: None,
        },

        "nip04.encrypt" | "nip04.decrypt" | "nip44.encrypt" | "nip44.decrypt" => Nip07Response {
            result: None,
            error: Some("Not implemented".to_string()),
        },

        _ => Nip07Response {
            result: None,
            error: Some(format!("Unknown method: {}", method)),
        },
    }
}

/// Tauri command wrapper for NIP-07 requests
#[tauri::command]
pub async fn nip07_request<R: Runtime>(
    app: AppHandle<R>,
    method: String,
    params: serde_json::Value,
    origin: String,
) -> Nip07Response {
    let worker_state = match app.try_state::<Arc<WorkerState>>() {
        Some(state) => state,
        None => {
            return Nip07Response {
                result: None,
                error: Some("WorkerState not found".to_string()),
            }
        }
    };
    let nip07_state = app.try_state::<Arc<Nip07State>>();
    let permissions = nip07_state.as_ref().map(|s| &*s.permissions);

    handle_nip07_request(&worker_state, permissions, &method, &params, &origin).await
}

/// Navigate an existing child webview to a new URL
#[tauri::command]
pub fn navigate_webview<R: Runtime>(
    app: AppHandle<R>,
    label: String,
    url: String,
) -> Result<(), String> {
    let webview = app
        .get_webview(&label)
        .ok_or_else(|| format!("Webview {} not found", label))?;
    let parsed = tauri::Url::parse(&url).map_err(|e| format!("Invalid URL: {}", e))?;
    webview
        .navigate(parsed)
        .map_err(|e| format!("Failed to navigate: {}", e))?;
    Ok(())
}

/// Navigate webview history without forcing a reload.
#[tauri::command]
pub fn webview_history<R: Runtime>(
    app: AppHandle<R>,
    label: String,
    direction: String,
) -> Result<(), String> {
    let webview = app
        .get_webview(&label)
        .ok_or_else(|| format!("Webview {} not found", label))?;

    let script = match direction.as_str() {
        "back" => "history.back()",
        "forward" => "history.forward()",
        _ => return Err("Invalid history direction".to_string()),
    };

    webview
        .eval(script)
        .map_err(|e| format!("Failed to navigate history: {}", e))?;
    Ok(())
}

/// Get the current URL of a child webview.
#[tauri::command]
pub fn webview_current_url<R: Runtime>(
    app: AppHandle<R>,
    label: String,
) -> Result<String, String> {
    let webview = app
        .get_webview(&label)
        .ok_or_else(|| format!("Webview {} not found", label))?;
    webview
        .url()
        .map(|url| url.to_string())
        .map_err(|e| format!("Failed to read webview URL: {}", e))
}
