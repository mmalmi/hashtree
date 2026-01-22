<script lang="ts">
  import { navigate } from '../lib/router.svelte';

  interface Props {
    nhash: string;
    subpath?: string;
  }

  let { nhash, subpath = '' }: Props = $props();

  let htreeServerUrl = $state<string | null>(null);
  let error = $state<string | null>(null);

  // Get htree server URL from Tauri
  $effect(() => {
    if (typeof window !== 'undefined' && '__TAURI__' in window) {
      const tauri = (window as unknown as { __TAURI__: { core: { invoke: (cmd: string) => Promise<string> } } }).__TAURI__;
      tauri.core.invoke('get_htree_server_url').then((url: string) => {
        htreeServerUrl = url;
      }).catch((e) => {
        console.error('[NHashViewer] Failed to get htree server URL:', e);
        error = 'Htree server not available';
      });
    } else {
      // Web version - use service worker path
      htreeServerUrl = '';
    }
  });

  // Build iframe URL: htree server serves /htree/nhash/... paths
  let iframeSrc = $derived.by(() => {
    if (htreeServerUrl === null) return null;

    // For Tauri: http://localhost:PORT/htree/nhash.../index.html
    // For web: /htree/nhash.../index.html (handled by SW)
    const filePath = subpath ? `/${subpath}` : '/index.html';
    return `${htreeServerUrl}/htree/${nhash}${filePath}`;
  });

  function goBack() {
    navigate('/');
  }
</script>

<div class="flex-1 flex flex-col">
  <!-- Toolbar -->
  <div class="h-10 flex items-center gap-2 px-2 bg-base-200 b-b b-base-300">
    <button class="btn btn-ghost btn-xs" onclick={goBack}>
      &larr; Back
    </button>
    <div class="flex-1 text-sm truncate text-base-content/60">
      /{nhash}{subpath ? '/' + subpath : ''}
    </div>
    <div class="text-xs text-success">
      Offline
    </div>
  </div>

  <!-- Content -->
  {#if error}
    <div class="flex-1 flex items-center justify-center text-error">
      {error}
    </div>
  {:else if iframeSrc}
    <iframe
      src={iframeSrc}
      class="flex-1 w-full border-0"
      sandbox="allow-scripts"
      title="Saved App"
    ></iframe>
  {:else}
    <div class="flex-1 flex items-center justify-center">
      <span class="i-lucide-loader-2 animate-spin text-2xl"></span>
    </div>
  {/if}
</div>
