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

  /** Get logical window size (converts from physical pixels using scale factor) */
  async function getLogicalWindowSize(win: Window): Promise<{ width: number; height: number }> {
    const physical = await win.innerSize();
    const scale = await win.scaleFactor();
    return { width: physical.width / scale, height: physical.height / scale };
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

      // Create webview with NIP-07 support via Rust command
      await invoke('create_nip07_webview', {
        label,
        url,
        x: 0,
        y: TOOLBAR_HEIGHT,
        width: windowSize.width,
        height: windowSize.height - TOOLBAR_HEIGHT,
      });

      webviewLabel = label;
      currentWebviewUrl = url;

      await new Promise(resolve => setTimeout(resolve, 100));

      // Show and focus the webview
      const childWebview = await Webview.getByLabel(label);
      if (childWebview) {
        await childWebview.show();
        await childWebview.setFocus();
      }

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

      // Listen for navigation events from child webview
      const { listen } = await import('@tauri-apps/api/event');
      unlistenLocation = await listen<{ label: string; url: string; source?: string }>('child-webview-location', (event) => {
        if (event.payload.label === webviewLabel) {
          if (!isAppRoute) return;
          const newUrl = event.payload.url;
          if (!newUrl || newUrl === currentWebviewUrl) return;
          currentWebviewUrl = newUrl;
          const encodedUrl = encodeURIComponent(newUrl);
          navigate(`/app/${encodedUrl}`);
        }
      });
    } catch (e) {
      console.error('[AppFrame] Failed to create webview:', e);
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
