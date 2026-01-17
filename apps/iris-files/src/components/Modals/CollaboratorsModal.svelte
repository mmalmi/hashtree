<script lang="ts" module>
  /**
   * Modal for managing document collaborators/editors
   * Features:
   * - UserRow display for collaborators
   * - Auto-save on add/remove (like React version)
   * - QR Scanner for adding npubs
   * - Search through followed users with B-Tree index
   */

  export interface CollaboratorsTarget {
    npubs: string[];
    onSave?: (npubs: string[]) => void;
  }

  let show = $state(false);
  let target = $state<CollaboratorsTarget | null>(null);

  export function open(t: CollaboratorsTarget) {
    target = t;
    show = true;
  }

  export function close() {
    show = false;
    target = null;
  }
</script>

<script lang="ts">
  import { open as openShareModal } from './ShareModal.svelte';
  import { nip19 } from 'nostr-tools';
  import { UserRow } from '../User';
  import { npubToPubkey, nostrStore } from '../../nostr';
  import { createFollowsStore } from '../../stores/follows';
  import { searchUsers, indexUsers, type UserIndexEntry } from '../../stores/searchIndex';
  import { getProfileSync } from '../../stores/profile';
  import QRScanner from '../QRScanner.svelte';
  import CopyText from '../CopyText.svelte';

  // Local state for editing
  let localNpubs = $state<string[]>([]);
  let newNpubInput = $state('');
  let addError = $state<string | null>(null);
  let pendingNpub = $state<string | null>(null);
  let showQRScanner = $state(false);
  let searchQuery = $state('');
  let showSearchResults = $state(false);

  // Get current user's pubkey for follows lookup (use $ prefix for reactivity)
  let userPubkey = $derived($nostrStore.pubkey);

  // Get current user's npub for sharing
  let userNpub = $derived(userPubkey ? nip19.npubEncode(userPubkey) : null);

  // Get followed users
  let followsStore = $derived(createFollowsStore(userPubkey));
  let follows = $state<string[]>([]);

  $effect(() => {
    if (!followsStore) {
      follows = [];
      return;
    }
    const unsub = followsStore.subscribe(value => {
      follows = value?.follows || [];
    });
    return () => {
      unsub();
      followsStore.destroy();
    };
  });

  // Index followed users when they change (with profile data if available)
  $effect(() => {
    if (follows.length === 0) return;

    const entries: UserIndexEntry[] = [];
    for (const pubkey of follows) {
      try {
        const npub = nip19.npubEncode(pubkey);
        const profile = getProfileSync(pubkey);
        entries.push({
          pubkey,
          npub,
          name: profile?.name,
          displayName: profile?.display_name,
          nip05: profile?.nip05,
        });
      } catch {
        // Skip invalid pubkeys
      }
    }

    if (entries.length > 0) {
      indexUsers(entries);
    }
  });

  // Async search results
  let searchResults = $state<UserIndexEntry[]>([]);
  let searchTimer: ReturnType<typeof setTimeout> | null = null;

  $effect(() => {
    const query = searchQuery.trim();
    if (query.length < 2) {
      searchResults = [];
      return;
    }

    // Debounce search
    if (searchTimer) clearTimeout(searchTimer);
    searchTimer = setTimeout(async () => {
      try {
        const results = await searchUsers(query, 10);
        // Filter out already added npubs
        searchResults = results.filter(item => !localNpubs.includes(item.npub));
      } catch {
        searchResults = [];
      }
    }, 150);

    return () => {
      if (searchTimer) clearTimeout(searchTimer);
    };
  });

  // Sync local state when modal opens
  $effect(() => {
    if (show && target) {
      newNpubInput = '';
      addError = null;
      pendingNpub = null;
      showQRScanner = false;
      searchQuery = '';
      showSearchResults = false;
    }
  });

  // Keep localNpubs in sync with target.npubs (reactive to external updates)
  $effect(() => {
    if (show && target) {
      localNpubs = [...target.npubs];
    }
  });

  // Check if we can edit (has onSave callback)
  let canEdit = $derived(!!target?.onSave);

  // Validate npub format
  function validateNpub(npub: string): { valid: boolean; error?: string } {
    if (!npub.trim()) {
      return { valid: false, error: 'Please enter an npub' };
    }

    if (!npub.startsWith('npub1') || npub.length !== 63) {
      return { valid: false, error: 'Invalid npub format. Must start with npub1 and be 63 characters.' };
    }

    try {
      const decoded = nip19.decode(npub);
      if (decoded.type !== 'npub') {
        return { valid: false, error: 'Invalid npub format' };
      }
      return { valid: true };
    } catch {
      return { valid: false, error: 'Invalid npub' };
    }
  }

  /** Extract npub from a scanned QR code text */
  function extractNpubFromScan(text: string): string | null {
    const cleaned = text.trim();

    // Direct npub match
    if (cleaned.startsWith('npub1') && cleaned.length === 63) {
      return cleaned;
    }

    // Try to find npub in the text (e.g., nostr:npub1...)
    const npubMatch = cleaned.match(/npub1[a-z0-9]{58}/i);
    if (npubMatch) {
      return npubMatch[0].toLowerCase();
    }

    // Try to decode hex pubkey
    if (/^[a-f0-9]{64}$/i.test(cleaned)) {
      try {
        return nip19.npubEncode(cleaned);
      } catch {
        return null;
      }
    }

    return null;
  }

  // Auto-detect valid npub as user types
  let detectedNpub = $derived.by(() => {
    if (pendingNpub) return null;
    const trimmed = newNpubInput.trim();
    if (!trimmed) return null;
    if (!trimmed.startsWith('npub1') || trimmed.length !== 63) return null;
    if (localNpubs.includes(trimmed)) return null;
    return trimmed;
  });

  function handlePrepareAdd() {
    const npub = newNpubInput.trim();
    const validation = validateNpub(npub);

    if (!validation.valid) {
      addError = validation.error || 'Invalid npub';
      return;
    }

    if (localNpubs.includes(npub)) {
      addError = 'This npub is already in the list.';
      return;
    }

    pendingNpub = npub;
    newNpubInput = '';
    addError = null;
    showSearchResults = false;
  }

  function handleConfirmAdd() {
    if (!pendingNpub || !target?.onSave) return;

    const newNpubs = [...localNpubs, pendingNpub];
    localNpubs = newNpubs;
    pendingNpub = null;
    // Auto-save immediately
    target.onSave(newNpubs);
  }

  function handleRemoveEditor(index: number) {
    const newNpubs = localNpubs.filter((_, i) => i !== index);
    localNpubs = newNpubs;
    // Auto-save immediately on remove
    if (target?.onSave) {
      target.onSave(newNpubs);
    }
  }

  function handleQRScan(result: string) {
    const npub = extractNpubFromScan(result);
    showQRScanner = false;

    if (npub) {
      if (localNpubs.includes(npub)) {
        addError = 'This npub is already in the list.';
        return;
      }
      pendingNpub = npub;
      addError = null;
    } else {
      addError = 'Could not find an npub in the scanned QR code.';
    }
  }

  function handleSearchSelect(result: UserIndexEntry) {
    if (localNpubs.includes(result.npub)) {
      addError = 'This npub is already in the list.';
      return;
    }
    pendingNpub = result.npub;
    searchQuery = '';
    showSearchResults = false;
    addError = null;
  }

  function handleClose() {
    close();
  }

  // Handle ESC key
  function handleKeyDown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      e.preventDefault();
      if (showQRScanner) {
        showQRScanner = false;
      } else if (pendingNpub) {
        pendingNpub = null;
      } else {
        handleClose();
      }
    }
  }

  $effect(() => {
    if (show) {
      document.addEventListener('keydown', handleKeyDown);
      return () => document.removeEventListener('keydown', handleKeyDown);
    }
  });
</script>

{#if show && target}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div class="fixed inset-0 z-50 flex items-center justify-center bg-black/70" onclick={handleClose} data-modal-backdrop>
    <div class="bg-surface-1 rounded-lg shadow-lg w-full max-w-md mx-4 border border-surface-3" onclick={(e) => e.stopPropagation()}>
      <!-- Header -->
      <div class="flex items-center justify-between px-4 py-3 b-b-1 b-b-solid b-b-surface-3">
        <h2 class="text-lg font-semibold">
          {canEdit ? 'Manage Editors' : 'Editors'}
        </h2>
        <button onclick={handleClose} class="btn-ghost p-1" aria-label="Close">
          <span class="i-lucide-x text-lg"></span>
        </button>
      </div>

      <!-- Content -->
      <div class="p-4 space-y-4">
        <p class="text-sm text-text-3">
          {canEdit
            ? 'Add editors by their npub to merge their edits into this document.'
            : 'Users who can edit this document. Their changes will be merged.'}
        </p>

        <!-- Share your npub to request edit access (only when not already an editor) -->
        {#if !canEdit && userNpub && !localNpubs.includes(userNpub)}
          <div class="bg-surface-2 rounded p-3 space-y-2">
            <p class="text-sm text-text-2">Share your npub with an editor to request access:</p>
            <div class="flex items-center gap-2">
              <CopyText
                text={userNpub}
                displayText={userNpub.slice(0, 12) + '...' + userNpub.slice(-6)}
                class="text-sm flex-1 min-w-0"
              />
              <button
                onclick={() => openShareModal(`${window.location.origin}/#/${userNpub}`)}
                class="btn-ghost p-2 shrink-0"
                title="Share with QR code"
              >
                <span class="i-lucide-share text-base"></span>
              </button>
            </div>
          </div>
        {/if}

        <!-- Current editors list -->
        {#if localNpubs.length > 0}
          <div class="space-y-2">
            <span class="text-sm font-medium">Current editors:</span>
            <ul class="space-y-1 list-none m-0 p-0">
              {#each localNpubs as npub, index (npub)}
                {@const pubkey = npubToPubkey(npub)}
                <li class="flex items-center gap-2 bg-surface-2 rounded px-3 py-2">
                  {#if pubkey}
                    <a href="#/{npub}" class="flex-1 min-w-0 hover:opacity-80">
                      <UserRow {pubkey} avatarSize={32} />
                    </a>
                  {:else}
                    <span class="i-lucide-user text-text-3"></span>
                    <span class="flex-1 text-sm font-mono truncate">{npub}</span>
                  {/if}
                  {#if canEdit}
                    <button
                      onclick={() => handleRemoveEditor(index)}
                      class="btn-ghost p-1 text-danger shrink-0"
                      title="Remove editor"
                    >
                      <span class="i-lucide-x"></span>
                    </button>
                  {/if}
                </li>
              {/each}
            </ul>
          </div>
        {:else}
          <div class="text-sm text-text-3 bg-surface-2 rounded px-3 py-2">
            No editors yet.{#if canEdit} Add one below.{/if}
          </div>
        {/if}

        <!-- Pending user preview -->
        {#if pendingNpub && canEdit}
          {@const pendingPubkey = npubToPubkey(pendingNpub)}
          <div class="space-y-2">
            <span class="text-sm font-medium">Add this editor?</span>
            <div class="bg-surface-2 rounded p-3 space-y-3">
              <div class="flex items-center gap-3">
                {#if pendingPubkey}
                  <UserRow pubkey={pendingPubkey} avatarSize={40} />
                {:else}
                  <span class="text-text-3 text-sm">Invalid npub</span>
                {/if}
              </div>
              <div class="flex gap-2">
                <button onclick={() => pendingNpub = null} class="btn-ghost flex-1 text-sm">
                  Cancel
                </button>
                <button
                  onclick={handleConfirmAdd}
                  class="btn-success flex-1 text-sm"
                  disabled={!pendingPubkey}
                >
                  Add User
                </button>
              </div>
            </div>
          </div>
        {/if}

        <!-- Auto-detected npub preview -->
        {#if detectedNpub && !pendingNpub && canEdit}
          {@const detectedPubkey = npubToPubkey(detectedNpub)}
          <div class="space-y-2">
            <span class="text-sm font-medium">Add this editor?</span>
            <div class="bg-surface-2 rounded p-3 space-y-3">
              <div class="flex items-center gap-3">
                {#if detectedPubkey}
                  <UserRow pubkey={detectedPubkey} avatarSize={40} />
                {:else}
                  <span class="text-text-3 text-sm">Invalid npub</span>
                {/if}
              </div>
              <div class="flex gap-2">
                <button onclick={() => newNpubInput = ''} class="btn-ghost flex-1 text-sm">
                  Cancel
                </button>
                <button
                  onclick={() => {
                    if (!detectedNpub || !target?.onSave) return;
                    const newNpubs = [...localNpubs, detectedNpub];
                    localNpubs = newNpubs;
                    newNpubInput = '';
                    addError = null;
                    target.onSave(newNpubs);
                  }}
                  class="btn-success flex-1 text-sm"
                  disabled={!detectedPubkey}
                >
                  Add User
                </button>
              </div>
            </div>
          </div>
        {/if}

        <!-- Add new editor (only if can edit and no pending) -->
        {#if canEdit && !pendingNpub}
          <div class="space-y-2">
            <span class="text-sm font-medium">Add editor:</span>

            <!-- Search through follows -->
            {#if follows.length > 0}
              <div class="relative">
                <div class="flex gap-2">
                  <div class="relative flex-1">
                    <span class="i-lucide-search absolute left-3 top-1/2 -translate-y-1/2 text-text-3 text-sm"></span>
                    <input
                      type="text"
                      bind:value={searchQuery}
                      oninput={() => {
                        showSearchResults = true;
                        addError = null;
                      }}
                      onfocus={() => showSearchResults = true}
                      placeholder="Search followed users..."
                      class="input w-full pl-9 text-sm"
                    />
                  </div>
                </div>

                <!-- Search results dropdown -->
                {#if showSearchResults && searchResults.length > 0}
                  <div class="absolute top-full left-0 right-0 mt-1 bg-surface-2 rounded border border-surface-3 shadow-lg z-10 max-h-48 overflow-auto">
                    {#each searchResults as result (result.pubkey)}
                      <button
                        onclick={() => handleSearchSelect(result)}
                        class="w-full px-3 py-2 hover:bg-surface-3 text-left"
                      >
                        <UserRow pubkey={result.pubkey} avatarSize={28} />
                      </button>
                    {/each}
                  </div>
                {/if}
              </div>
            {/if}

            <!-- Divider or label -->
            {#if follows.length > 0}
              <div class="flex items-center gap-2 text-xs text-text-3">
                <span class="flex-1 h-px bg-surface-3"></span>
                <span>or paste npub</span>
                <span class="flex-1 h-px bg-surface-3"></span>
              </div>
            {/if}

            <!-- Manual npub input - hide when detectedNpub shows UserPreview -->
            {#if !detectedNpub}
              <div class="flex gap-2">
                <input
                  type="text"
                  bind:value={newNpubInput}
                  placeholder="npub1..."
                  class="input flex-1 font-mono text-sm"
                  onkeydown={(e) => e.key === 'Enter' && handlePrepareAdd()}
                />
                <button
                  onclick={() => showQRScanner = true}
                  class="btn-ghost px-2"
                  title="Scan QR code"
                >
                  <span class="i-lucide-qr-code text-lg"></span>
                </button>
                <button
                  onclick={handlePrepareAdd}
                  class="btn-success px-3"
                  disabled={!newNpubInput.trim()}
                >
                  Add
                </button>
              </div>
            {/if}
            {#if addError}
              <p class="text-sm text-danger">{addError}</p>
            {/if}
          </div>
        {/if}
      </div>

      <!-- Footer -->
      <div class="flex justify-end gap-2 px-4 py-3 b-t-1 b-t-solid b-t-surface-3">
        <button onclick={handleClose} class="btn-ghost">
          Close
        </button>
      </div>
    </div>
  </div>

  <!-- QR Scanner overlay -->
  {#if showQRScanner}
    <QRScanner
      onScanSuccess={handleQRScan}
      onClose={() => showQRScanner = false}
    />
  {/if}
{/if}
