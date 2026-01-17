<script lang="ts" module>
  /**
   * Modal for forking a directory as a new tree
   */
  import type { CID, TreeVisibility } from 'hashtree';

  export interface ForkTarget {
    dirCid: CID;
    suggestedName: string;
  }

  let show = $state(false);
  let forkTarget = $state<ForkTarget | null>(null);
  let modalInput = $state('');

  export function open(dirCid: CID, suggestedName: string) {
    forkTarget = { dirCid, suggestedName };
    modalInput = suggestedName;
    show = true;
  }

  export function close() {
    show = false;
    forkTarget = null;
    modalInput = '';
  }
</script>

<script lang="ts">
  import type { TreeVisibility } from 'hashtree';
  import { forkTree } from '../../actions/tree';
  import VisibilityPicker from './VisibilityPicker.svelte';

  let isForking = $state(false);
  let error = $state('');
  let inputRef: HTMLInputElement | undefined = $state();
  let visibility = $state<TreeVisibility>('public');

  // Reset visibility when modal opens
  $effect(() => {
    if (show) {
      visibility = 'public';
    }
  });

  // Focus input when modal opens
  $effect(() => {
    if (show && inputRef) {
      inputRef.focus();
      inputRef.select();
    }
  });

  async function handleFork() {
    if (!forkTarget || !modalInput.trim()) return;

    const name = modalInput.trim();
    if (!name) {
      error = 'Name is required';
      return;
    }

    isForking = true;
    error = '';

    try {
      const result = await forkTree(forkTarget.dirCid, name, visibility);
      if (result.success) {
        close();
      } else {
        error = 'Failed to create fork. Please try again.';
      }
    } catch (e) {
      console.error('Fork error:', e);
      error = e instanceof Error ? e.message : 'Failed to create fork';
    } finally {
      isForking = false;
    }
  }

  function handleKeyDown(e: KeyboardEvent) {
    if (e.key === 'Enter' && !isForking) {
      e.preventDefault();
      handleFork();
    }
  }
</script>

{#if show && forkTarget}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div class="fixed inset-0 z-50 flex items-center justify-center bg-black/50" onclick={close}>
    <div class="bg-surface-1 rounded-lg shadow-lg p-6 w-full max-w-md mx-4" onclick={(e) => e.stopPropagation()}>
      <h2 class="text-lg font-semibold mb-2">Fork as New Folder</h2>
      <p class="text-text-2 text-sm mb-4">
        Create a new top-level folder with a copy of this directory's contents.
      </p>

      <div class="mb-4">
        <label for="fork-name" class="block text-sm font-medium text-text-2 mb-1">
          Folder name
        </label>
        <input
          bind:this={inputRef}
          id="fork-name"
          type="text"
          bind:value={modalInput}
          onkeydown={handleKeyDown}
          placeholder="Enter folder name..."
          class="w-full px-3 py-2 bg-surface-2 border border-surface-3 rounded text-text-1 placeholder:text-text-3 focus:outline-none focus:border-accent"
          disabled={isForking}
        />
      </div>

      <div class="mb-4">
        <VisibilityPicker value={visibility} onchange={(v) => visibility = v} />
      </div>

      {#if error}
        <p class="text-danger text-sm mb-4">{error}</p>
      {/if}

      <div class="flex justify-end gap-2">
        <button
          onclick={close}
          class="btn-ghost px-4 py-2"
          disabled={isForking}
        >
          Cancel
        </button>
        <button
          onclick={handleFork}
          class="btn-primary px-4 py-2"
          disabled={isForking || !modalInput.trim()}
        >
          {#if isForking}
            <span class="i-lucide-loader-2 animate-spin mr-2"></span>
            Forking...
          {:else}
            <span class="i-lucide-git-fork mr-2"></span>
            Fork
          {/if}
        </button>
      </div>
    </div>
  </div>
{/if}
