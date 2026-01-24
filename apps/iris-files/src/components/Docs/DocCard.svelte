<script lang="ts">
  /**
   * DocCard - A4 aspect ratio document card for docs grid
   * Shows thumbnail preview if available via Service Worker URL
   */
  import { onDestroy } from 'svelte';
  import VisibilityIcon from '../VisibilityIcon.svelte';
  import { Avatar } from '../User';
  import { getNpubFileUrl, onHtreePrefixReady } from '../../lib/mediaUrl';
  import { getThumbnailFilename } from '../../lib/yjs/thumbnail';

  interface Props {
    href: string;
    displayName: string;
    ownerPubkey?: string | null;
    ownerNpub?: string | null;
    treeName?: string | null;
    visibility?: string;
    rootHashHex?: string | null;
  }

  let { href, displayName, ownerPubkey, ownerNpub, treeName, visibility, rootHashHex }: Props = $props();

  // Use SW URL for thumbnail - SW handles caching with stale-while-revalidate
  // In Tauri, the URL prefix may not be available immediately, so we rebuild when it's ready
  let prefixVersion = $state(0);
  let retryToken = $state(0);
  let thumbnailUrl = $derived.by(() => {
    void prefixVersion;
    void retryToken;
    if (!ownerNpub || !treeName) return null;
    const baseUrl = getNpubFileUrl(ownerNpub, treeName, getThumbnailFilename());
    const params: string[] = [];
    if (rootHashHex) {
      params.push(`v=${rootHashHex.slice(0, 8)}`);
    }
    if (retryToken) {
      params.push(`r=${retryToken}`);
    }
    if (params.length === 0) return baseUrl;
    const separator = baseUrl.includes('?') ? '&' : '?';
    return `${baseUrl}${separator}${params.join('&')}`;
  });

  // Subscribe to prefix ready event (handles Tauri async initialization)
  onHtreePrefixReady(() => {
    prefixVersion++;
  });

  const MAX_THUMBNAIL_RETRIES = 8;
  const RETRY_BACKOFF_MAX_MS = 8000;

  let thumbnailError = $state(false);
  let lastThumbnailUrl = $state<string | null>(null);
  let retryCount = $state(0);
  let retryTimeout: ReturnType<typeof setTimeout> | null = null;

  $effect(() => {
    if (thumbnailUrl && thumbnailUrl !== lastThumbnailUrl) {
      thumbnailError = false;
      lastThumbnailUrl = thumbnailUrl;
      retryCount = 0;
      if (retryTimeout) {
        clearTimeout(retryTimeout);
        retryTimeout = null;
      }
    }
  });

  onDestroy(() => {
    if (retryTimeout) {
      clearTimeout(retryTimeout);
    }
  });

  function handleThumbnailError() {
    thumbnailError = true;
    if (retryCount >= MAX_THUMBNAIL_RETRIES) return;

    retryCount += 1;
    const delayMs = Math.min(1000 * (2 ** (retryCount - 1)), RETRY_BACKOFF_MAX_MS);

    if (retryTimeout) clearTimeout(retryTimeout);
    retryTimeout = setTimeout(() => {
      thumbnailError = false;
      retryToken += 1;
    }, delayMs);
  }
</script>

<a
  {href}
  class="bg-surface-1 rounded-lg b-1 b-solid b-surface-3 hover:b-accent hover:shadow-md transition-all no-underline flex flex-col overflow-hidden"
  style="aspect-ratio: 210 / 297"
>
  <div class="flex-1 flex items-center justify-center overflow-hidden">
    {#if thumbnailUrl && !thumbnailError}
      <img
        src={thumbnailUrl}
        alt=""
        class="w-full h-full object-cover object-top"
        onerror={handleThumbnailError}
      />
    {:else}
      <span class="i-lucide-file-text text-4xl text-accent"></span>
    {/if}
  </div>
  <div class="p-2 bg-surface-1">
    <div class="flex items-center gap-1.5">
      {#if ownerPubkey}
        <Avatar pubkey={ownerPubkey} size={16} />
      {/if}
      <VisibilityIcon {visibility} class="text-text-3 text-xs" />
      <h3 class="text-sm font-medium text-text-1 truncate">{displayName}</h3>
    </div>
  </div>
</a>
