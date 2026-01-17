<script lang="ts">
  /**
   * DocCard - A4 aspect ratio document card for docs grid
   * Shows thumbnail preview if available via Service Worker URL
   */
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
  }

  let { href, displayName, ownerPubkey, ownerNpub, treeName, visibility }: Props = $props();

  // Use SW URL for thumbnail - SW handles caching with stale-while-revalidate
  // In Tauri, the URL prefix may not be available immediately, so we rebuild when it's ready
  let prefixVersion = $state(0);
  let thumbnailUrl = $derived(
    ownerNpub && treeName ? getNpubFileUrl(ownerNpub, treeName, getThumbnailFilename()) + (prefixVersion ? '' : '') : null
  );

  // Subscribe to prefix ready event (handles Tauri async initialization)
  onHtreePrefixReady(() => {
    prefixVersion++;
  });

  let thumbnailError = $state(false);
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
        onerror={() => thumbnailError = true}
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
