<script lang="ts" module>
  /**
   * BlossomPushModal - Push directory/file contents to Blossom servers
   * Optionally republishes tree event to Nostr if pubkey+treeName provided
   */
  import type { CID } from 'hashtree';

  export interface BlossomPushTarget {
    cid: CID;
    name: string;
    isDirectory: boolean;
    pubkey?: string; // If provided with treeName, republish original event to Nostr
    treeName?: string;
  }

  let show = $state(false);
  let target = $state<BlossomPushTarget | null>(null);

  export function open(cid: CID, name: string, isDirectory: boolean, pubkey?: string, treeName?: string) {
    target = { cid, name, isDirectory, pubkey, treeName };
    show = true;
  }

  export function close() {
    show = false;
    target = null;
  }
</script>

<script lang="ts">
  import { getWorkerAdapter } from '../../lib/workerInit';
  import { reencryptSingleTree } from '../../migrations/reencrypt';
  import { nip19 } from 'nostr-tools';

  // State
  let phase = $state<'confirm' | 'pushing' | 'reencrypting' | 'done'>('confirm');
  let progress = $state({ current: 0, total: 0 });
  let pushResult = $state<{ pushed: number; skipped: number; failed: number } | null>(null);
  let error = $state<string | null>(null);
  let reencrypted = $state(false);
  let reencryptAttempted = $state(false);

  // Reset state when modal opens/closes
  $effect(() => {
    if (!show) {
      phase = 'confirm';
      progress = { current: 0, total: 0 };
      pushResult = null;
      error = null;
      nostrPublished = false;
      reencrypted = false;
      reencryptAttempted = false;
    }
  });

  // Handle Escape key
  $effect(() => {
    if (!show) return;
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && phase !== 'pushing') {
        close();
      }
    };
    document.addEventListener('keydown', handleKeyDown);
    return () => document.removeEventListener('keydown', handleKeyDown);
  });

  let nostrPublished = $state(false);

  function hasEncryptionError(errors?: string[]): boolean {
    if (!errors) return false;
    return errors.some(e => e.includes('not encrypted') || e.includes('Unique:'));
  }

  async function startPush() {
    if (!target) return;

    phase = 'pushing';
    error = null;
    nostrPublished = false;

    try {
      const adapter = getWorkerAdapter();
      if (!adapter) {
        throw new Error('Worker not initialized');
      }

      // If CID has no key, it's unencrypted - re-encrypt BEFORE trying to push
      if (!target.cid.key && target.pubkey && target.treeName && !reencryptAttempted) {
        console.log('[BlossomPush] CID has no key - re-encrypting before push...');
        reencryptAttempted = true;
        await handleReencrypt();
        return;
      }

      // Set up progress callback
      adapter.onBlossomPushProgress((treeName, current, total) => {
        progress = { current, total };
      });

      adapter.onBlossomPushComplete(() => {
        // Progress complete
      });

      // Push to blossom via worker
      let result = await adapter.pushToBlossom(
        target.cid.hash,
        target.cid.key,
        target.name
      );

      // Check for encryption errors - automatically re-encrypt and retry (only once)
      if (result.failed > 0 && hasEncryptionError(result.errors) && target.pubkey && target.treeName && !reencryptAttempted) {
        console.log('[BlossomPush] Encryption error detected, auto re-encrypting...');
        reencryptAttempted = true;
        await handleReencrypt();
        return;
      }

      pushResult = result;
      phase = 'done';

      // If pubkey+treeName provided, republish original event to Nostr without blocking UI
      if (target.pubkey && target.treeName) {
        void adapter.republishTree(target.pubkey, target.treeName)
          .then((published) => {
            nostrPublished = published;
          })
          .catch((err) => {
            console.warn('[BlossomPush] Failed to republish tree:', err);
          });
      }
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
      phase = 'done';
    }
  }

  async function handleReencrypt() {
    if (!target || !target.pubkey || !target.treeName) return;

    phase = 'reencrypting';
    error = null;

    try {
      // Convert pubkey to npub
      const npub = nip19.npubEncode(target.pubkey);

      // Force re-encryption even though CID has a key
      // reencryptSingleTree already publishes to Nostr AND pushes to Blossom
      const newCid = await reencryptSingleTree(
        npub,
        target.treeName,
        target.cid,
        'public', // visibility
        true // force re-encryption
      );

      if (!newCid) {
        error = 'Re-encryption failed';
        phase = 'done';
        return;
      }

      reencrypted = true;
      nostrPublished = true; // reencryptSingleTree already published
      // Update target with new CID
      target = { ...target, cid: newCid };
      // reencryptSingleTree already pushed to Blossom, set result
      pushResult = { pushed: 1, skipped: 0, failed: 0 };

      phase = 'done';
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
      phase = 'done';
    }
  }
</script>

{#if show && target}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div
    class="fixed inset-0 bg-black/70 flex-center z-1000 overflow-auto"
    onclick={(e) => {
      if (e.target === e.currentTarget && phase !== 'pushing') close();
    }}
    data-modal-backdrop
    data-testid="blossom-push-modal-backdrop"
  >
    <div
      class="bg-surface-1 sm:rounded-lg overflow-hidden w-screen sm:w-[32rem] sm:max-w-[90vw] sm:border border-surface-3 max-h-[90vh] flex flex-col"
      data-testid="blossom-push-modal"
    >
      <!-- Header -->
      <div class="p-4 border-b border-surface-3 flex items-center justify-between">
        <h2 class="text-lg font-semibold flex items-center gap-2">
          <span class="i-lucide-upload-cloud"></span>
          Push to File Servers
        </h2>
        {#if phase !== 'pushing'}
          <button onclick={close} class="btn-ghost p-1" title="Close">
            <span class="i-lucide-x"></span>
          </button>
        {/if}
      </div>

      <!-- Content -->
      <div class="p-4 overflow-auto flex-1">
        {#if phase === 'confirm'}
          <!-- Confirmation -->
          <div class="mb-4">
            <p class="text-sm text-text-3">
              Push <span class="text-text-1 font-medium">{target.name}</span>
              {target.isDirectory ? ' (directory)' : ''} to configured file servers?
            </p>
          </div>

          <div class="flex justify-end gap-2">
            <button onclick={close} class="btn-ghost px-4 py-2">
              Cancel
            </button>
            <button
              onclick={startPush}
              class="btn-primary px-4 py-2 flex items-center gap-2"
              data-testid="start-push-btn"
            >
              <span class="i-lucide-upload"></span>
              Push
            </button>
          </div>

        {:else if phase === 'pushing'}
          <!-- Progress -->
          <div class="space-y-4">
            <div class="flex items-center gap-2">
              <span class="i-lucide-loader-2 animate-spin text-accent"></span>
              <span class="text-sm">Pushing chunks...</span>
            </div>

            <div class="bg-surface-2 rounded p-3">
              <div class="text-xs text-text-3 mb-1">
                {progress.current} / {progress.total} chunks
              </div>
              <div class="h-2 bg-surface-3 rounded-full overflow-hidden">
                <div
                  class="h-full bg-accent transition-all"
                  style="width: {progress.total > 0 ? (progress.current / progress.total * 100) : 0}%"
                ></div>
              </div>
            </div>
          </div>

        {:else if phase === 'reencrypting'}
          <!-- Re-encrypting -->
          <div class="space-y-4">
            <div class="flex items-center gap-2">
              <span class="i-lucide-loader-2 animate-spin text-accent"></span>
              <span class="text-sm">Re-encrypting data...</span>
            </div>
            <p class="text-xs text-text-3">
              This may take a while for large trees.
            </p>
          </div>

        {:else}
          <!-- Results -->
          <div class="space-y-4">
            {#if error}
              <div class="bg-danger/10 text-danger rounded p-3 text-sm">
                {error}
              </div>
            {:else if pushResult}
              <!-- Summary -->
              <div class="grid grid-cols-3 gap-2 text-center">
                <div class="bg-surface-2 rounded p-2">
                  <div class="text-lg font-semibold text-success">{pushResult.pushed}</div>
                  <div class="text-xs text-text-3">Uploaded</div>
                </div>
                <div class="bg-surface-2 rounded p-2">
                  <div class="text-lg font-semibold text-text-3">{pushResult.skipped}</div>
                  <div class="text-xs text-text-3">Already exist</div>
                </div>
                <div class="bg-surface-2 rounded p-2">
                  <div class="text-lg font-semibold text-danger">{pushResult.failed}</div>
                  <div class="text-xs text-text-3">Failed</div>
                </div>
              </div>

              {#if reencrypted}
                <div class="flex items-center gap-2 text-sm text-success">
                  <span class="i-lucide-check"></span>
                  <span>Data re-encrypted</span>
                </div>
              {/if}

              {#if nostrPublished}
                <div class="flex items-center gap-2 text-sm text-success">
                  <span class="i-lucide-check"></span>
                  <span>Published to Nostr relays</span>
                </div>
              {/if}

              <button
                onclick={close}
                class="btn-primary w-full py-2"
              >
                Done
              </button>
            {:else}
              <button
                onclick={close}
                class="btn-primary w-full py-2"
              >
                Done
              </button>
            {/if}
          </div>
        {/if}
      </div>
    </div>
  </div>
{/if}
