<script lang="ts">
  /**
   * DocsHome - Home page for docs.iris.to
   * Shows combination of recent docs and user's own docs
   * Similar to Google Docs home page
   */
  import { nip19 } from 'nostr-tools';
  import { SvelteSet } from 'svelte/reactivity';
  import { nostrStore } from '../../nostr';
  import { recentsStore, clearRecentsByPrefix, type RecentItem } from '../../stores/recents';
  import { createTreesStore } from '../../stores';
  import { open as openCreateModal } from '../Modals/CreateModal.svelte';
  import DocCard from './DocCard.svelte';

  /** Encode tree name for use in URL path */
  function encodeTreeNameForUrl(treeName: string): string {
    return encodeURIComponent(treeName);
  }

  // Get current user
  let userNpub = $derived($nostrStore.npub);
  let userPubkey = $derived($nostrStore.pubkey);
  let isLoggedIn = $derived($nostrStore.isLoggedIn);

  // Get recents - only show trees starting with "docs/"
  let recents = $derived($recentsStore);
  let recentDocs = $derived(
    recents
      .filter(r => r.treeName?.startsWith('docs/'))
      .map(r => ({
        key: r.path,
        displayName: r.treeName ? r.treeName.slice(5) : r.label, // Remove "docs/" prefix
        ownerPubkey: r.npub ? npubToPubkey(r.npub) : null,
        ownerNpub: r.npub,
        treeName: r.treeName,
        visibility: r.visibility,
        href: buildRecentHref(r),
        timestamp: r.timestamp,
      }))
  );

  // Get user's own trees
  let treesStore = $derived(createTreesStore(userNpub));
  let trees = $state<Array<{ name: string; visibility?: string; rootHash?: string; linkKey?: string }>>([]);

  $effect(() => {
    const store = treesStore;
    const unsub = store.subscribe(value => {
      trees = value;
    });
    return unsub;
  });

  // User's own docs - only show trees starting with "docs/"
  let ownDocs = $derived(
    trees
      .filter(t => t.name.startsWith('docs/'))
      .map(t => ({
        key: `/${userNpub}/${t.name}`,
        displayName: t.name.slice(5), // Remove "docs/" prefix for display
        ownerPubkey: userPubkey,
        ownerNpub: userNpub,
        treeName: t.name,
        visibility: t.visibility,
        href: `#/${userNpub}/${encodeTreeNameForUrl(t.name)}${t.linkKey ? `?k=${t.linkKey}` : ''}`,
        timestamp: 0, // Own docs don't have timestamp, will be sorted after recents
      }))
  );

  // Merge recents and own docs, deduplicate by treeName, recents first
  let allDocs = $derived.by(() => {
    const seen = new SvelteSet<string>();
    const result: typeof recentDocs = [];

    // Add recents first (they have timestamps)
    for (const doc of recentDocs) {
      const dedupeKey = `${doc.ownerNpub}/${doc.treeName}`;
      if (!seen.has(dedupeKey)) {
        seen.add(dedupeKey);
        result.push(doc);
      }
    }

    // Add own docs that aren't already in recents
    for (const doc of ownDocs) {
      const dedupeKey = `${doc.ownerNpub}/${doc.treeName}`;
      if (!seen.has(dedupeKey)) {
        seen.add(dedupeKey);
        result.push(doc);
      }
    }

    return result.slice(0, 30);
  });

  function npubToPubkey(npub: string): string | null {
    try {
      const decoded = nip19.decode(npub);
      if (decoded.type === 'npub') {
        return decoded.data as string;
      }
    } catch {}
    return null;
  }

  function buildRecentHref(item: RecentItem): string {
    // Encode treeName in path: /npub/treeName -> /npub/encodedTreeName
    const encodedPath = item.treeName
      ? `/${item.npub}/${encodeURIComponent(item.treeName)}`
      : item.path;
    const base = `#${encodedPath}`;
    return item.linkKey ? `${base}?k=${item.linkKey}` : base;
  }

  // Create new document
  function createNewDoc() {
    if (!userNpub) {
      alert('Please sign in to create a document');
      return;
    }
    openCreateModal('document');
  }
</script>

<div class="flex-1 overflow-auto">
  <div class="max-w-4xl mx-auto p-6">
    <!-- Header with clear button when there are recent docs -->
    {#if recentDocs.length > 0}
      <div class="flex items-center justify-between mb-4">
        <h2 class="text-lg font-semibold text-text-1">Recent Documents</h2>
        <button
          class="btn-ghost text-xs text-text-3 hover:text-text-2"
          onclick={() => clearRecentsByPrefix('docs/')}
        >
          Clear Recent
        </button>
      </div>
    {/if}

    <!-- Documents grid with A4 aspect ratio cards -->
    <div class="grid gap-4" style="grid-template-columns: repeat(auto-fill, minmax(10rem, 1fr))">
      <!-- New Document card (only when logged in) -->
      {#if isLoggedIn}
        <div
          onclick={createNewDoc}
          class="bg-surface-1 rounded-lg b-1 b-dashed b-surface-3 hover:b-accent transition-colors cursor-pointer flex flex-col items-center justify-center gap-2"
          style="aspect-ratio: 210 / 297"
          role="button"
          tabindex="0"
        >
          <span class="i-lucide-plus text-4xl text-accent"></span>
          <span class="text-sm text-text-2">New Document</span>
        </div>
      {/if}

      <!-- Documents -->
      {#each allDocs as doc (doc.href)}
        <DocCard
          href={doc.href}
          displayName={doc.displayName}
          ownerPubkey={doc.ownerPubkey}
          ownerNpub={doc.ownerNpub}
          treeName={doc.treeName}
          visibility={doc.visibility}
        />
      {/each}
    </div>
  </div>
</div>
