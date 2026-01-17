<script lang="ts">
  import { nip19 } from 'nostr-tools';
  import { nostrStore } from '../../nostr';
  import { appStore, formatBytes, updateStorageStats } from '../../store';
  import { settingsStore, DEFAULT_STORAGE_SETTINGS } from '../../stores/settings';
  import { isTauri } from '../../tauri';

  let isLoggedIn = $derived($nostrStore.isLoggedIn);
  let myPubkey = $derived($nostrStore.pubkey);

  // Storage settings
  let storageSettings = $derived($settingsStore.storage);
  let editingStorage = $state(false);

  // Stats
  let appState = $derived($appStore);
  let stats = $derived(appState.stats);

  // Worker backend info
  let workerBackend = $derived(isTauri() ? 'Tauri (Rust)' : 'Web Worker');

  // Republish state
  let isRepublishing = $state(false);
  let republishResult = $state<number | null>(null);
  let blossomProgress = $state<{ treeName: string; current: number; total: number } | null>(null);
  let republishPrefix = $state('');
  let encryptionErrors = $state<string[]>([]);
  let isReencrypting = $state(false);
  let reencryptProgress = $state<{ current: number; total: number } | null>(null);

  // Load stats on mount
  $effect(() => {
    updateStorageStats();
  });

  async function handleRepublish() {
    if (isRepublishing) return;
    isRepublishing = true;
    republishResult = null;
    blossomProgress = null;
    encryptionErrors = [];
    try {
      const { getWorkerAdapter } = await import('../../workerAdapter');
      const adapter = getWorkerAdapter();
      if (adapter) {
        adapter.onBlossomPushProgress((treeName, current, total) => {
          blossomProgress = { treeName, current, total };
        });
        adapter.onBlossomPushComplete(() => {
          blossomProgress = null;
        });

        const prefix = republishPrefix.trim() ? encodeURIComponent(republishPrefix.trim()) : undefined;
        const result = await adapter.republishTrees(prefix);
        republishResult = result.count;
        blossomProgress = null;

        if (result.encryptionErrors && result.encryptionErrors.length > 0) {
          encryptionErrors = result.encryptionErrors;
          await handleReencrypt();
        } else {
          setTimeout(() => (republishResult = null), 5000);
        }
      }
    } catch (e) {
      console.error('Failed to republish:', e);
    } finally {
      isRepublishing = false;
      blossomProgress = null;
    }
  }

  async function handleReencrypt() {
    if (isReencrypting || encryptionErrors.length === 0 || !myPubkey) return;
    isReencrypting = true;
    reencryptProgress = { current: 0, total: encryptionErrors.length };

    try {
      const { reencryptSingleTree } = await import('../../migrations/reencrypt');
      const npub = nip19.npubEncode(myPubkey);

      for (let i = 0; i < encryptionErrors.length; i++) {
        const treeName = encryptionErrors[i];
        reencryptProgress = { current: i + 1, total: encryptionErrors.length };

        try {
          const { getTreeRootSync } = await import('../../stores/treeRoot');
          const cid = getTreeRootSync(npub, treeName);
          if (!cid) continue;
          await reencryptSingleTree(npub, treeName, cid, 'public', true);
        } catch (e) {
          console.error('[Settings] Failed to re-encrypt:', treeName, e);
        }
      }

      encryptionErrors = [];
      setTimeout(() => (republishResult = null), 5000);
    } catch (e) {
      console.error('Failed to re-encrypt:', e);
    } finally {
      isReencrypting = false;
      reencryptProgress = null;
    }
  }
</script>

<div class="p-4 space-y-6 max-w-2xl mx-auto">
  <!-- Worker Backend -->
  <div>
    <h3 class="text-xs font-medium text-muted uppercase tracking-wide mb-3">
      Worker Backend
    </h3>
    <div class="bg-surface-2 rounded p-3 text-sm">
      <div class="flex justify-between">
        <span class="text-muted">Type</span>
        <span class="text-text-1 flex items-center gap-1.5">
          {#if isTauri()}
            <span class="w-2 h-2 rounded-full bg-success"></span>
          {:else}
            <span class="w-2 h-2 rounded-full bg-info"></span>
          {/if}
          {workerBackend}
        </span>
      </div>
    </div>
  </div>

  <!-- Local Storage -->
  <div>
    <div class="flex items-center justify-between mb-1">
      <h3 class="text-xs font-medium text-muted uppercase tracking-wide">
        Local Storage
      </h3>
      <button onclick={() => editingStorage = !editingStorage} class="btn-ghost text-xs">
        {editingStorage ? 'Done' : 'Edit'}
      </button>
    </div>
    <p class="text-xs text-text-3 mb-3">IndexedDB cache for downloaded content</p>
    <div class="bg-surface-2 rounded p-3 text-sm space-y-2">
      <div class="flex justify-between">
        <span class="text-muted">Items</span>
        <span class="text-text-1" data-testid="storage-items">{stats.items.toLocaleString()}</span>
      </div>
      <div class="flex justify-between">
        <span class="text-muted">Size</span>
        <span class="text-text-1" data-testid="storage-size">{formatBytes(stats.bytes)}</span>
      </div>
      <div class="flex justify-between">
        <span class="text-muted">Limit</span>
        <span class="text-text-1" data-testid="storage-limit">{formatBytes(storageSettings.maxBytes)}</span>
      </div>
      {#if stats.bytes > 0}
        <div class="flex justify-between">
          <span class="text-muted">Usage</span>
          <span class="text-text-1">{Math.round((stats.bytes / storageSettings.maxBytes) * 100)}%</span>
        </div>
      {/if}
    </div>
    {#if editingStorage}
      <div class="mt-2 bg-surface-2 rounded p-3">
        <label class="flex flex-col gap-1">
          <span class="text-xs text-text-3">Storage limit (MB)</span>
          <input
            type="number"
            min="100"
            max="10000"
            step="100"
            value={Math.round(storageSettings.maxBytes / 1024 / 1024)}
            onchange={(e) => {
              const mb = parseInt(e.currentTarget.value) || 1024;
              settingsStore.setStorageSettings({ maxBytes: mb * 1024 * 1024 });
            }}
            class="input text-sm"
            data-testid="storage-limit-input"
          />
        </label>
        <p class="text-xs text-text-3 mt-2">Oldest data will be evicted when limit is exceeded</p>
      </div>
      <button onclick={() => { settingsStore.resetStorageSettings(); editingStorage = false; }} class="btn-ghost mt-2 text-xs text-text-3">
        Reset to default ({Math.round(DEFAULT_STORAGE_SETTINGS.maxBytes / 1024 / 1024)} MB)
      </button>
    {/if}
  </div>

  <!-- Data Sync -->
  {#if isLoggedIn}
    <div>
      <h3 class="text-xs font-medium text-muted uppercase tracking-wide mb-1">
        Data Sync
      </h3>
      <p class="text-xs text-text-3 mb-3">Republish your directories if they're not syncing to other devices</p>
      <div class="bg-surface-2 rounded p-3 space-y-3">
        <div>
          <label for="republish-prefix" class="text-xs text-text-3 mb-1 block">Prefix filter (optional)</label>
          <input
            id="republish-prefix"
            type="text"
            bind:value={republishPrefix}
            placeholder="e.g. photos/ or projects/"
            class="input text-sm w-full"
            disabled={isRepublishing}
          />
        </div>
        <button
          onclick={handleRepublish}
          disabled={isRepublishing || isReencrypting}
          class="btn-ghost flex items-center gap-2 text-sm w-full justify-start"
        >
          {#if isReencrypting}
            <span class="i-lucide-loader-2 animate-spin"></span>
            <span>Re-encrypting {reencryptProgress?.current}/{reencryptProgress?.total}...</span>
          {:else if isRepublishing}
            <span class="i-lucide-loader-2 animate-spin"></span>
            <span>Republishing...</span>
          {:else if republishResult !== null}
            <span class="i-lucide-check text-success"></span>
            <span>Republished {republishResult} directories</span>
          {:else}
            <span class="i-lucide-upload-cloud"></span>
            <span>Republish {republishPrefix.trim() ? `"${republishPrefix.trim()}*"` : 'all'} directories to relays</span>
          {/if}
        </button>
        {#if blossomProgress}
          <div class="mt-2 flex items-center gap-2 text-xs text-text-3">
            <span class="i-lucide-loader-2 animate-spin text-accent"></span>
            <span>Uploading {blossomProgress.treeName}: {blossomProgress.current} chunks...</span>
          </div>
        {/if}
      </div>
    </div>
  {/if}
</div>
