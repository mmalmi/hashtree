<script lang="ts">
  import { onMount } from 'svelte';
  import { SvelteURL } from 'svelte/reactivity';
  import { isNHash, isNPath } from 'hashtree';
  import NostrLogin from './components/NostrLogin.svelte';
  import ConnectivityIndicator from './components/ConnectivityIndicator.svelte';
  import BandwidthIndicator from './components/BandwidthIndicator.svelte';
  import WalletLink from './components/WalletLink.svelte';
  import Toast from './components/Toast.svelte';
  import IrisRouter from './components/IrisRouter.svelte';
  import ShareModal, { open as openShareModal } from './components/Modals/ShareModal.svelte';
  import { currentPath, initRouter, navigate, refresh } from './lib/router.svelte';
  import { settingsStore } from './stores/settings';
  import { appsStore } from './stores/apps';
  import { fetchPWA } from './lib/pwaFetcher';
  import { savePWAToHashtree } from './lib/pwaSaver';
  import { isTauri } from './tauri';

  let showConnectivity = $derived($settingsStore.pools.showConnectivity ?? true);
  let showBandwidth = $derived($settingsStore.pools.showBandwidth ?? false);

  // Navigation history for back/forward
  let historyStack = $state<string[]>([]);
  let historyIndex = $state(-1);
  let lastNonAppPath = $state<string | null>(null);
  let isAppRoute = $derived($currentPath.startsWith('/app/'));
  let canGoBack = $derived(historyIndex > 0 || (isAppRoute && isTauri()));
  let canGoForward = $derived(historyIndex < historyStack.length - 1 || (isAppRoute && isTauri()));
  let pendingWebviewHistory = $state<{
    id: number;
    direction: 'back' | 'forward';
    path: string;
  } | null>(null);
  let webviewHistoryRequestId = $state(0);
  let lastWebviewHistoryAction = $state<'back' | 'forward' | null>(null);
  let lastWebviewHistoryAt = $state(0);

  // Address bar
  let addressValue = $state('');
  let isAddressFocused = $state(false);
  let addressInputEl: HTMLInputElement | null = $state(null);

  // Bookmark state
  let isSaving = $state(false);
  let currentUrl = $derived.by(() => {
    const path = $currentPath;
    if (path.startsWith('/app/')) {
      try {
        return decodeURIComponent(path.slice(5));
      } catch {
        return null;
      }
    }
    if (path.startsWith('/nhash')) {
      return path;
    }
    return null;
  });
  let isBookmarked = $derived(currentUrl ? $appsStore.some(app => app.url === currentUrl) : false);
  let canBookmark = $derived(currentUrl !== null);

  const CHILD_WEBVIEW_LABEL = 'app-frame';

  function isEditableTarget(target: EventTarget | null): boolean {
    if (target instanceof HTMLElement) {
      if (target.closest('input, textarea, select')) return true;
      if (target.isContentEditable) return true;
    }
    return false;
  }

  function isEditableEventTarget(event: KeyboardEvent): boolean {
    if (isEditableTarget(event.target)) return true;
    if (typeof event.composedPath === 'function') {
      for (const entry of event.composedPath()) {
        if (isEditableTarget(entry as EventTarget)) return true;
      }
    }
    return false;
  }

  function normalizeAppPath(path: string): string | null {
    if (!path.startsWith('/app/')) return null;
    try {
      const decoded = decodeURIComponent(path.slice(5));
      const url = new SvelteURL(decoded);
      if ((!url.hash || url.hash === '#') && url.pathname.endsWith('.html')) {
        url.hash = '#/';
      }
      return url.href;
    } catch {
      return null;
    }
  }

  function pathsMatch(a: string, b: string): boolean {
    if (a === b) return true;
    const normalizedA = normalizeAppPath(a);
    const normalizedB = normalizeAppPath(b);
    if (normalizedA && normalizedB) return normalizedA === normalizedB;
    return false;
  }

  function replaceHistoryEntry(stack: string[], index: number, path: string): string[] {
    const next = [...stack];
    next[index] = path;
    return next;
  }

  function scheduleWebviewFallback(direction: 'back' | 'forward', requestPath: string) {
    const requestId = webviewHistoryRequestId + 1;
    webviewHistoryRequestId = requestId;
    pendingWebviewHistory = { id: requestId, direction, path: requestPath };
    const targetPath =
      direction === 'back'
        ? historyStack[historyIndex - 1]
        : historyStack[historyIndex + 1];
    const fallbackDelay = targetPath?.startsWith('/app/') ? 1500 : 300;

    setTimeout(() => {
      if (!pendingWebviewHistory || pendingWebviewHistory.id !== requestId) return;
      if ($currentPath !== requestPath || !isTauri() || !isAppRoute) {
        pendingWebviewHistory = null;
        return;
      }

      if (targetPath) {
        navigate(targetPath);
      } else if (direction === 'back') {
        if (lastNonAppPath && lastNonAppPath !== $currentPath) {
          navigate(lastNonAppPath);
        } else {
          navigate('/');
        }
      }

      pendingWebviewHistory = null;
    }, fallbackDelay);
  }

  // Convert internal path to display value for address bar
  function pathToDisplayValue(path: string): string {
    if (path === '/') return '';
    if (path.startsWith('/app/')) {
      try {
        const url = decodeURIComponent(path.slice(5));
        return url.replace(/^https?:\/\//, '');
      } catch {
        return path;
      }
    }
    return path;
  }

  // Track path changes for history
  $effect(() => {
    const path = $currentPath;
    if (path) {
      if (!path.startsWith('/app/')) {
        lastNonAppPath = path;
      }
      if (historyStack.length === 0) {
        historyStack = [path];
        historyIndex = 0;
      } else if (!pathsMatch(historyStack[historyIndex], path)) {
        if (historyIndex > 0 && pathsMatch(historyStack[historyIndex - 1], path)) {
          const nextIndex = historyIndex - 1;
          historyIndex = nextIndex;
          historyStack = replaceHistoryEntry(historyStack, nextIndex, path);
        } else if (historyIndex + 1 < historyStack.length && pathsMatch(historyStack[historyIndex + 1], path)) {
          const nextIndex = historyIndex + 1;
          historyIndex = nextIndex;
          historyStack = replaceHistoryEntry(historyStack, nextIndex, path);
        } else {
          historyStack = [...historyStack.slice(0, historyIndex + 1), path];
          historyIndex = historyStack.length - 1;
        }
      }
    }
    if (!isAddressFocused) {
      addressValue = pathToDisplayValue(path);
    }
    if (pendingWebviewHistory && path !== pendingWebviewHistory.path) {
      pendingWebviewHistory = null;
    }
  });

  function goBack() {
    if (isTauri() && isAppRoute) {
      scheduleWebviewFallback('back', $currentPath);
      void requestWebviewHistory('back');
      return;
    }
    if (!canGoBack) return;
    historyIndex--;
    navigate(historyStack[historyIndex]);
  }

  function goForward() {
    if (isTauri() && isAppRoute) {
      scheduleWebviewFallback('forward', $currentPath);
      void requestWebviewHistory('forward');
      return;
    }
    if (!canGoForward) return;
    historyIndex++;
    navigate(historyStack[historyIndex]);
  }

  async function requestWebviewHistory(direction: 'back' | 'forward') {
    try {
      lastWebviewHistoryAction = direction;
      lastWebviewHistoryAt = Date.now();
      const { invoke } = await import('@tauri-apps/api/core');
      await invoke('webview_history', { label: CHILD_WEBVIEW_LABEL, direction });
    } catch (error) {
      console.warn('[Webview] Failed to navigate history:', error);
    }
  }

  // Check if value starts with a hashtree identifier (nhash1, npath1, npub1)
  // These can have paths like nhash1.../index.html or npub1.../treename/file.txt
  function isHashtreeIdentifier(value: string): boolean {
    // Extract the first segment (before any slash)
    const firstSegment = value.split('/')[0];
    return isNHash(firstSegment) || isNPath(firstSegment) || (firstSegment.startsWith('npub1') && firstSegment.length >= 63);
  }

  function handleAddressSubmit() {
    const value = addressValue.trim();
    if (value) {
      if (value.startsWith('http://') || value.startsWith('https://')) {
        navigate(`/app/${encodeURIComponent(value)}`);
      } else if (value.startsWith('/')) {
        navigate(value);
      } else if (isHashtreeIdentifier(value)) {
        // nhash1.../path, npath1..., npub1.../treename/path - navigate as internal route
        navigate(`/${value}`);
      } else if (value.includes('.') && !value.includes(' ')) {
        navigate(`/app/${encodeURIComponent('https://' + value)}`);
      } else {
        navigate(`/${value}`);
      }
    } else {
      navigate('/');
    }
    addressInputEl?.blur();
    isAddressFocused = false;
  }

  function getShareableUrl(): string {
    const url = new SvelteURL(window.location.href);
    // Replace localhost/dev URLs with production
    if (url.hostname === 'localhost' || url.hostname === '127.0.0.1') {
      url.hostname = 'iris.to';
      url.port = '';
      url.protocol = 'https:';
    }
    return url.toString();
  }

  function handleShare() {
    openShareModal(getShareableUrl());
  }

  async function handleBookmark() {
    if (!currentUrl || isSaving) return;

    if (isBookmarked) {
      // Remove bookmark
      appsStore.remove(currentUrl);
      return;
    }

    // For external URLs, save to hashtree first
    if (currentUrl.startsWith('http')) {
      isSaving = true;
      try {
        const pwaInfo = await fetchPWA(currentUrl);
        const nhashUrl = await savePWAToHashtree(pwaInfo);
        const appName = pwaInfo.manifest?.name || pwaInfo.manifest?.short_name || new SvelteURL(currentUrl).hostname;

        appsStore.add({
          url: nhashUrl,
          name: appName,
          addedAt: Date.now(),
        });

        // Navigate to saved version
        navigate(nhashUrl);
      } catch (error) {
        console.error('[Bookmark] Failed to save:', error);
        // Fallback: just bookmark the external URL
        appsStore.add({
          url: currentUrl,
          name: new SvelteURL(currentUrl).hostname,
          addedAt: Date.now(),
        });
      } finally {
        isSaving = false;
      }
    } else {
      // nhash URL - just bookmark it
      appsStore.add({
        url: currentUrl,
        name: 'Saved App',
        addedAt: Date.now(),
      });
    }
  }

  async function handleToolbarMouseDown(e: MouseEvent) {
    if (!isTauri() || e.buttons !== 1) return;
    const target = e.target as HTMLElement;
    if (target.closest('button, input, a, [role="button"]')) return;
    const { getCurrentWindow } = await import('@tauri-apps/api/window');
    getCurrentWindow().startDragging();
  }

  onMount(() => {
    initRouter();
  });

  // Mouse back/forward buttons and Cmd/Ctrl + arrow navigation (Tauri only)
  $effect(() => {
    if (!isTauri()) return;

    function handleMouseUp(e: MouseEvent) {
      // Mouse button 3 = back, button 4 = forward
      if (e.button === 3) {
        e.preventDefault();
        goBack();
      } else if (e.button === 4) {
        e.preventDefault();
        goForward();
      }
    }

    function handleKeyDown(e: KeyboardEvent) {
      if (isEditableEventTarget(e)) return;
      const isMac = navigator.platform.toUpperCase().includes('MAC');
      const modifier = isMac ? e.metaKey : e.ctrlKey;

      if (modifier && !e.shiftKey && !e.altKey) {
        if (e.key === 'ArrowLeft') {
          e.preventDefault();
          goBack();
        } else if (e.key === 'ArrowRight') {
          e.preventDefault();
          goForward();
        }
      }
    }

    window.addEventListener('mouseup', handleMouseUp);
    window.addEventListener('keydown', handleKeyDown);

    return () => {
      window.removeEventListener('mouseup', handleMouseUp);
      window.removeEventListener('keydown', handleKeyDown);
    };
  });

  $effect(() => {
    if (!isTauri()) return;

    let unlistenNavigate: (() => void) | null = null;

    (async () => {
      const { listen } = await import('@tauri-apps/api/event');
      unlistenNavigate = await listen<{ action: string; label?: string }>('child-webview-navigate', (event) => {
        const fromChild = typeof event.payload.label === 'string';
        const action = event.payload.action === 'forward' ? 'forward' : 'back';
        if (fromChild) {
          if (!isAppRoute || !isTauri()) return;
          if (pendingWebviewHistory?.direction === action) return;
          if (lastWebviewHistoryAction === action && Date.now() - lastWebviewHistoryAt < 800) return;
          scheduleWebviewFallback(action, $currentPath);
          return;
        }
        if (isEditableTarget(document.activeElement)) return;
        if (action === 'back') {
          goBack();
        } else {
          goForward();
        }
      });
    })();

    return () => {
      unlistenNavigate?.();
    };
  });
</script>

<div class="h-screen flex flex-col bg-surface-0 overscroll-none">
  <!-- Safari-style toolbar -->
  <div
    class="h-12 shrink-0 flex items-center gap-2 px-3 bg-surface-1 border-b border-surface-2"
    style="padding-left: 80px;"
    onmousedown={handleToolbarMouseDown}
  >
    <!-- Back/Forward/Home buttons -->
    <div class="flex items-center gap-1">
      <button
        class="btn-circle btn-ghost"
        onclick={goBack}
        disabled={!canGoBack}
        title="Back"
      >
        <span class="i-lucide-chevron-left text-lg"></span>
      </button>
      <button
        class="btn-circle btn-ghost"
        onclick={goForward}
        disabled={!canGoForward}
        title="Forward"
      >
        <span class="i-lucide-chevron-right text-lg"></span>
      </button>
      <button
        class="btn-circle btn-ghost"
        onclick={() => navigate('/')}
        disabled={$currentPath === '/'}
        title="Home"
      >
        <span class="i-lucide-home text-lg"></span>
      </button>
    </div>

    <!-- Address bar with bookmark star -->
    <div class="flex-1 flex justify-center">
      <div class="w-full max-w-lg flex items-center gap-2 px-3 py-1 rounded-full bg-surface-0 b-1 b-solid b-surface-3 transition-colors {isAddressFocused ? 'b-accent' : ''}">
        <!-- Bookmark/Star button -->
        <button
          class="shrink-0 text-text-3 hover:text-text-1 disabled:opacity-30"
          onclick={handleBookmark}
          disabled={!canBookmark || isSaving}
          title={isBookmarked ? 'Remove bookmark' : 'Add bookmark'}
        >
          {#if isSaving}
            <span class="i-lucide-loader-2 animate-spin"></span>
          {:else if isBookmarked}
            <span class="i-lucide-star text-yellow-500 fill-yellow-500"></span>
          {:else}
            <span class="i-lucide-star"></span>
          {/if}
        </button>
        <span class="i-lucide-search text-sm text-muted shrink-0"></span>
        <input
          type="text"
          bind:this={addressInputEl}
          bind:value={addressValue}
          onfocus={() => isAddressFocused = true}
          onblur={() => isAddressFocused = false}
          onkeydown={(e) => {
            if (e.key === 'Enter') {
              handleAddressSubmit();
            } else if ((e.metaKey || e.ctrlKey) && e.key === 'v') {
              // Handle paste explicitly for Tauri compatibility
              e.preventDefault();
              navigator.clipboard.readText().then((text) => {
                addressValue = text;
              });
            }
          }}
          placeholder="Search or enter address"
          class="bg-transparent border-none outline-none text-sm text-text-1 placeholder:text-muted flex-1 text-center"
        />
        {#if currentUrl}
          <button
            class="shrink-0 text-text-3 hover:text-text-1"
            onclick={refresh}
            title="Refresh"
          >
            <span class="i-lucide-refresh-cw text-sm"></span>
          </button>
        {/if}
      </div>
    </div>

    <!-- Right side: share, connectivity, wallet, avatar -->
    <div class="flex items-center gap-2">
      <button
        class="btn-circle btn-ghost"
        onclick={handleShare}
        title="Share"
      >
        <span class="i-lucide-share text-lg"></span>
      </button>
      {#if showBandwidth}
        <BandwidthIndicator />
      {/if}
      {#if showConnectivity}
        <ConnectivityIndicator />
      {/if}
      <WalletLink />
      <NostrLogin />
    </div>
  </div>

  <!-- Content area -->
  <main class="flex-1 flex flex-col overflow-auto">
    <IrisRouter />
  </main>
</div>

<Toast />
<ShareModal />
