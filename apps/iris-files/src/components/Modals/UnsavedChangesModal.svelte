<script lang="ts" module>
  /**
   * Modal for confirming unsaved changes before closing editor
   */
  export interface UnsavedChangesTarget {
    onSave: () => Promise<void> | void;
    onDiscard: () => void;
    fileName?: string;
  }

  let show = $state(false);
  let target = $state<UnsavedChangesTarget | null>(null);

  export function open(t: UnsavedChangesTarget) {
    target = t;
    show = true;
  }

  export function close() {
    show = false;
    target = null;
  }
</script>

<script lang="ts">
  let saving = $state(false);

  async function handleSave() {
    if (!target) return;
    saving = true;
    try {
      await target.onSave();
    } finally {
      saving = false;
      close();
    }
  }

  function handleDiscard() {
    if (!target) return;
    target.onDiscard();
    close();
  }

  // Handle ESC key
  function handleKeyDown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      close();
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
  <div class="fixed inset-0 z-50 flex items-center justify-center bg-black/50" onclick={close} data-modal-backdrop>
    <div class="bg-surface-1 rounded-lg shadow-lg p-6 w-full max-w-md mx-4" onclick={(e) => e.stopPropagation()}>
      <h2 class="text-lg font-semibold mb-2">Unsaved Changes</h2>
      <p class="text-text-2 mb-4">
        {#if target.fileName}
          <strong>{target.fileName}</strong> has unsaved changes.
        {:else}
          You have unsaved changes.
        {/if}
        What would you like to do?
      </p>
      <div class="flex justify-end gap-2">
        <button onclick={close} class="btn-ghost">
          Cancel
        </button>
        <button onclick={handleDiscard} class="btn-ghost text-error">
          Don't Save
        </button>
        <button onclick={handleSave} disabled={saving} class="btn-success">
          {saving ? 'Saving...' : 'Save'}
        </button>
      </div>
    </div>
  </div>
{/if}
