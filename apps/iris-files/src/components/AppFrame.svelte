<script lang="ts">
  import { onDestroy } from 'svelte';
  import { isTauri } from '../tauri';
  import { currentPath, navigate } from '../lib/router.svelte';
  import type { Window } from '@tauri-apps/api/window';

  interface Props {
    appUrl: string;
  }

  let { appUrl }: Props = $props();

  const TOOLBAR_HEIGHT = 48;
  const WEBVIEW_LABEL = 'app-frame';
  const useNativeWebview = isTauri();
  let isAppRoute = $derived($currentPath.startsWith('/app/'));
  let webviewLabel: string | null = null;
  let currentWebviewUrl: string | null = null;
  let pendingUrl: string | null = null;
  let isCreating = false;
  let unlistenResize: (() => void) | null = null;
  let unlistenLocation: (() => void) | null = null;
  let locationListenerPromise: Promise<void> | null = null;
  let urlPollTimer: ReturnType<typeof setInterval> | null = null;
  let urlPollInFlight = false;
  let coreInvokePromise: Promise<typeof import('@tauri-apps/api/core')> | null = null;
  let hasObservedHashUrl = false;

  function parseUrl(value: string): URL | null {
    try {
      return new URL(value);
    } catch {
      return null;
    }
  }

  function shouldIgnoreLocation(newUrl: string): boolean {
    if (newUrl.startsWith('about:blank')) return true;
    const expected = currentWebviewUrl ?? appUrl;
    if (!expected) return false;
    const expectedUrl = parseUrl(expected);
    const nextUrl = parseUrl(newUrl);
    if (!expectedUrl || !nextUrl) return false;
    if (
      expectedUrl.origin === nextUrl.origin &&
      expectedUrl.pathname === nextUrl.pathname &&
      expectedUrl.search === nextUrl.search &&
      expectedUrl.hash &&
      !nextUrl.hash
    ) {
      return true;
    }
    return false;
  }

  function shouldExitAppForUrl(newUrl: string): boolean {
    if (!currentWebviewUrl) return false;
    const current = parseUrl(currentWebviewUrl);
    const next = parseUrl(newUrl);
    if (!current || !next) return false;
    if (!hasObservedHashUrl) return false;
    if (current.protocol !== 'tauri:') return false;
    if (!current.hash) return false;
    if (newUrl.startsWith('about:blank')) return true;
    return (
      current.origin === next.origin &&
      current.pathname === next.pathname &&
      current.search === next.search &&
      !next.hash
    );
  }

  /** Get logical window size (converts from physical pixels using scale factor) */
  async function getLogicalWindowSize(win: Window): Promise<{ width: number; height: number }> {
    const physical = await win.innerSize();
    const scale = await win.scaleFactor();
    return { width: physical.width / scale, height: physical.height / scale };
  }

  async function ensureLocationListener() {
    if (!useNativeWebview) return;
    if (unlistenLocation) return;
    if (!locationListenerPromise) {
      locationListenerPromise = (async () => {
        const { listen } = await import('@tauri-apps/api/event');
        unlistenLocation = await listen<{ label: string; url: string; source?: string }>('child-webview-location', (event) => {
          if (event.payload.label !== WEBVIEW_LABEL) return;
          if (!isAppRoute) return;
          const newUrl = event.payload.url;
          if (!newUrl || newUrl === currentWebviewUrl || shouldIgnoreLocation(newUrl)) return;
          currentWebviewUrl = newUrl;
          const encodedUrl = encodeURIComponent(newUrl);
          navigate(`/app/${encodedUrl}`);
        });
      })();
    }
    await locationListenerPromise;
    locationListenerPromise = null;
  }

  async function getCoreInvoke() {
    if (!coreInvokePromise) {
      coreInvokePromise = import('@tauri-apps/api/core');
    }
    const { invoke } = await coreInvokePromise;
    return invoke;
  }

  async function pollWebviewUrl() {
    if (!useNativeWebview || !webviewLabel || !isAppRoute) return;
    if (urlPollInFlight) return;
    urlPollInFlight = true;
    try {
      const invoke = await getCoreInvoke();
      const url = await invoke<string>('webview_current_url', { label: webviewLabel });
      if (!url || url === currentWebviewUrl) return;
      if (url.includes('#')) {
        hasObservedHashUrl = true;
      }
      if (shouldExitAppForUrl(url)) {
        navigate('/');
        return;
      }
      if (shouldIgnoreLocation(url)) return;
      currentWebviewUrl = url;
      const encodedUrl = encodeURIComponent(url);
      navigate(`/app/${encodedUrl}`);
    } catch (e) {
      console.warn('[AppFrame] Failed to poll webview URL:', e);
    } finally {
      urlPollInFlight = false;
    }
  }

  function startUrlPolling() {
    if (urlPollTimer) return;
    urlPollTimer = setInterval(() => {
      void pollWebviewUrl();
    }, 300);
  }

  function stopUrlPolling() {
    if (urlPollTimer) {
      clearInterval(urlPollTimer);
      urlPollTimer = null;
    }
  }

  async function createWebview(url: string) {
    if (!useNativeWebview) return;
    if (isCreating) {
      pendingUrl = url;
      return;
    }
    isCreating = true;

    // Close existing webview if any
    await destroyWebview();
    await ensureLocationListener();

    try {
      const { invoke } = await import('@tauri-apps/api/core');
      const { getCurrentWindow } = await import('@tauri-apps/api/window');
      const { getCurrentWebview, Webview } = await import('@tauri-apps/api/webview');

      const currentWindow = getCurrentWindow();
      const windowSize = await getLogicalWindowSize(currentWindow);

      // Resize main webview to toolbar height to make room for child webview
      // Note: On Linux, toolbar clicks may not work due to GTK multiwebview limitations
      const mainWebview = getCurrentWebview();
      await mainWebview.setSize({ type: 'Logical', width: windowSize.width, height: TOOLBAR_HEIGHT });

      const label = WEBVIEW_LABEL;
      webviewLabel = label;
      currentWebviewUrl = url;

      // Create webview with NIP-07 support via Rust command
      await invoke('create_nip07_webview', {
        label,
        url,
        x: 0,
        y: TOOLBAR_HEIGHT,
        width: windowSize.width,
        height: windowSize.height - TOOLBAR_HEIGHT,
      });

      await new Promise(resolve => setTimeout(resolve, 100));

      // Show and focus the webview
      const childWebview = await Webview.getByLabel(label);
      if (childWebview) {
        await childWebview.show();
        await childWebview.setFocus();
      }
      startUrlPolling();

      // Listen for window resize
      unlistenResize = await currentWindow.onResized(async () => {
        if (webviewLabel) {
          try {
            const { width, height } = await getLogicalWindowSize(currentWindow);
            // Resize main webview
            await mainWebview.setSize({ type: 'Logical', width, height: TOOLBAR_HEIGHT });
            // Resize child webview
            const child = await Webview.getByLabel(webviewLabel);
            if (child) {
              await child.setSize({ type: 'Logical', width, height: height - TOOLBAR_HEIGHT });
            }
          } catch {}
        }
      });

    } catch (e) {
      console.error('[AppFrame] Failed to create webview:', e);
      webviewLabel = null;
      currentWebviewUrl = null;
    }
    isCreating = false;
    if (pendingUrl && pendingUrl !== currentWebviewUrl) {
      const nextUrl = pendingUrl;
      pendingUrl = null;
      await navigateWebview(nextUrl);
    }
  }

  async function navigateWebview(url: string) {
    if (!useNativeWebview) return;
    if (!webviewLabel) {
      pendingUrl = url;
      await createWebview(url);
      return;
    }
    if (url === currentWebviewUrl) return;

    try {
      const { invoke } = await import('@tauri-apps/api/core');
      currentWebviewUrl = url;
      await invoke('navigate_webview', { label: webviewLabel, url });
    } catch (e) {
      console.error('[AppFrame] Failed to navigate webview:', e);
    }
  }

  async function destroyWebview() {
    stopUrlPolling();
    if (unlistenResize) {
      unlistenResize();
      unlistenResize = null;
    }

    if (unlistenLocation) {
      unlistenLocation();
      unlistenLocation = null;
    }

    if (webviewLabel) {
      try {
        const { Webview } = await import('@tauri-apps/api/webview');
        const { getCurrentWebview } = await import('@tauri-apps/api/webview');
        const { getCurrentWindow } = await import('@tauri-apps/api/window');

        // Close the child webview
        const childWebview = await Webview.getByLabel(webviewLabel);
        if (childWebview) {
          await childWebview.close();
        }

        // Restore main webview to full size
        const mainWebview = getCurrentWebview();
        const { width, height } = await getLogicalWindowSize(getCurrentWindow());
        await mainWebview.setSize({ type: 'Logical', width, height });
      } catch {}
      webviewLabel = null;
      currentWebviewUrl = null;
      hasObservedHashUrl = false;
      pendingUrl = null;
    }
  }

  // Create webview when URL changes
  $effect(() => {
    if (appUrl && useNativeWebview) {
      if (!webviewLabel) {
        pendingUrl = appUrl;
        createWebview(appUrl);
      } else if (appUrl !== currentWebviewUrl) {
        navigateWebview(appUrl);
      }
    }
  });

  onDestroy(() => {
    destroyWebview();
  });
</script>

{#if useNativeWebview}
  <!-- Placeholder div - actual content is in native webview -->
  <div class="flex-1 w-full bg-surface-0"></div>
{:else}
  <!-- Browser fallback: sandboxed iframe -->
  <iframe
    src={appUrl}
    class="flex-1 w-full border-0"
    sandbox="allow-scripts allow-same-origin allow-forms allow-popups allow-popups-to-escape-sandbox"
    title="App"
    referrerpolicy="no-referrer"
  ></iframe>
{/if}
