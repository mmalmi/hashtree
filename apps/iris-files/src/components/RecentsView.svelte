<script lang="ts">
  /**
   * RecentsView - shows recently accessed files and trees
   */
  import { nip19 } from 'nostr-tools';
  import { recentsStore, clearRecents, type RecentItem } from '../stores/recents';
  import { TreeRow } from './ui';

  let recents = $derived($recentsStore);

  function npubToPubkey(npub: string): string | null {
    try {
      const decoded = nip19.decode(npub);
      if (decoded.type === 'npub') {
        return decoded.data as string;
      }
    } catch {}
    return null;
  }

  function formatTime(timestamp: number): string {
    const now = Date.now();
    const diff = now - timestamp;
    const minutes = Math.floor(diff / 60000);
    const hours = Math.floor(diff / 3600000);
    const days = Math.floor(diff / 86400000);

    if (minutes < 1) return 'Just now';
    if (minutes < 60) return `${minutes}m ago`;
    if (hours < 24) return `${hours}h ago`;
    if (days < 7) return `${days}d ago`;
    return new Date(timestamp).toLocaleDateString();
  }

  function isFolder(item: RecentItem): boolean {
    return item.type === 'tree' || item.type === 'dir' || item.type === 'hash';
  }

  function buildHref(item: RecentItem): string {
    // Encode treeName in path: /npub/treeName -> /npub/encodedTreeName
    // For playlist videos, encode treeName and videoId separately
    // For files, use the full path which includes the file
    let encodedPath: string;
    if (item.type === 'file' && item.path) {
      // Files use the full path (includes file name)
      // Extract parts and encode properly: /npub/treeName/path/to/file
      const pathParts = item.path.split('/').filter(Boolean);
      if (pathParts.length >= 2) {
        const npub = pathParts[0];
        const rest = pathParts.slice(1).map(p => encodeURIComponent(p)).join('/');
        encodedPath = `/${npub}/${rest}`;
      } else {
        encodedPath = item.path;
      }
    } else if (item.treeName) {
      encodedPath = item.videoId
        ? `/${item.npub}/${encodeURIComponent(item.treeName)}/${encodeURIComponent(item.videoId)}`
        : `/${item.npub}/${encodeURIComponent(item.treeName)}`;
    } else {
      encodedPath = item.path;
    }
    const base = `#${encodedPath}`;
    return item.linkKey ? `${base}?k=${item.linkKey}` : base;
  }
</script>

<div class="flex-1 flex flex-col min-h-0" data-testid="recents-view">
  <div class="h-10 shrink-0 px-4 border-b border-surface-3 flex items-center">
    <span class="text-sm font-medium text-text-1">Recent</span>
    {#if recents.length > 0}
      <span class="ml-2 text-xs text-text-3">{recents.length}</span>
      <button
        class="ml-auto btn-ghost text-xs px-2 py-1"
        onclick={() => clearRecents()}
        title="Clear recents"
      >
        Clear
      </button>
    {/if}
  </div>
  <div class="flex-1 overflow-auto">
    {#if recents.length === 0}
      <div class="p-4 text-muted text-sm">
        No recent items
      </div>
    {:else}
      <div>
        {#each recents as item (item.path)}
          <TreeRow
            href={buildHref(item)}
            name={item.label}
            isFolder={isFolder(item)}
            ownerPubkey={item.npub ? npubToPubkey(item.npub) : null}
            showHashIcon={item.type === 'hash'}
            hasKey={item.hasKey}
            visibility={item.visibility}
            time={formatTime(item.timestamp)}
            noHover
            class={item.type === 'hash' ? 'font-mono' : ''}
          />
        {/each}
      </div>
    {/if}
  </div>
</div>
