<script lang="ts">
  /**
   * FileEditor - textarea editor for text files
   * Port of React Viewer edit functionality
   */
  import { saveFile } from '../../actions';
  import { open as openUnsavedChangesModal } from '../Modals/UnsavedChangesModal.svelte';
  import { settingsStore } from '../../stores/settings';

  interface Props {
    fileName: string;
    initialContent: string;
    onDone: () => void;
  }

  let { fileName, initialContent, onDone }: Props = $props();

  let editContent = $state('');
  let savedContent = $state(''); // Track last saved content
  let saving = $state(false);
  let initialized = false;

  // Initialize content from prop
  $effect(() => {
    if (!initialized && initialContent !== undefined) {
      editContent = initialContent;
      savedContent = initialContent;
      initialized = true;
    }
  });

  // Get autosave setting from store
  let autoSaveEnabled = $derived($settingsStore.editor.autoSave);

  // Track if content has been modified since last save
  let isDirty = $derived(editContent !== savedContent);

  async function handleSave() {
    saving = true;
    await saveFile(fileName, editContent);
    savedContent = editContent; // Update saved content after successful save
    saving = false;
    triggerSavedIndicator();
  }

  // Show "Saved!" for 2 seconds after saving
  let showSaved = $state(false);
  let savedTimer: ReturnType<typeof setTimeout> | null = null;

  function triggerSavedIndicator() {
    showSaved = true;
    if (savedTimer) clearTimeout(savedTimer);
    savedTimer = setTimeout(() => {
      showSaved = false;
    }, 2000);
  }

  // Autosave: debounce save after 1 second of no typing
  let autoSaveTimer: ReturnType<typeof setTimeout> | null = null;

  $effect(() => {
    // Only autosave if enabled and content is dirty
    if (!autoSaveEnabled || !isDirty) {
      if (autoSaveTimer) {
        clearTimeout(autoSaveTimer);
        autoSaveTimer = null;
      }
      return;
    }

    // Clear previous timer
    if (autoSaveTimer) {
      clearTimeout(autoSaveTimer);
    }

    // Set new timer for autosave (1 second debounce)
    autoSaveTimer = setTimeout(() => {
      handleSave();
    }, 1000);

    return () => {
      if (autoSaveTimer) {
        clearTimeout(autoSaveTimer);
        autoSaveTimer = null;
      }
    };
  });

  function handleClose() {
    if (isDirty) {
      openUnsavedChangesModal({
        fileName,
        onSave: async () => {
          await handleSave();
          onDone();
        },
        onDiscard: () => {
          onDone();
        },
      });
    } else {
      onDone();
    }
  }

  function handleKeyDown(e: KeyboardEvent) {
    // Ctrl/Cmd + S to save
    if ((e.ctrlKey || e.metaKey) && e.key === 's') {
      e.preventDefault();
      handleSave();
    }
  }

  // Global keyboard handler (works even when textarea is focused)
  $effect(() => {
    const handleGlobalKeyDown = (e: KeyboardEvent) => {
      // ESC to close (with unsaved changes check)
      if (e.key === 'Escape') {
        // Don't handle if a modal is open (check if any modal backdrop exists)
        if (document.querySelector('[data-modal-backdrop]')) return;

        e.preventDefault();
        handleClose();
      }
    };

    document.addEventListener('keydown', handleGlobalKeyDown);
    return () => document.removeEventListener('keydown', handleGlobalKeyDown);
  });
</script>

<div class="flex-1 flex flex-col min-h-0 bg-surface-0">
  <!-- Header -->
  <div class="shrink-0 px-3 py-2 border-b border-surface-3 flex flex-wrap items-center justify-between gap-2 bg-surface-1">
    <div class="flex items-center gap-2 min-w-0">
      <span class="i-lucide-file-text text-text-2"></span>
      <span class="font-medium text-text-1">{fileName}</span>
      <span class="text-xs text-muted">
        (editing{#if isDirty && !autoSaveEnabled} - unsaved{/if})
      </span>
    </div>
    <div class="flex items-center gap-3">
      <label class="flex items-center gap-1.5 text-sm text-text-2 cursor-pointer select-none">
        <input
          type="checkbox"
          checked={autoSaveEnabled}
          onchange={(e) => settingsStore.setEditorSettings({ autoSave: e.currentTarget.checked })}
          class="w-4 h-4 accent-accent cursor-pointer"
        />
        Autosave
      </label>
      <button onclick={handleSave} disabled={saving || !isDirty} class="btn-success relative">
        <!-- Invisible text to maintain button width -->
        <span class="invisible">Saving...</span>
        <!-- Visible text positioned absolutely -->
        <span class="absolute inset-0 flex items-center justify-center">
          {saving ? 'Saving...' : showSaved ? 'Saved' : 'Save'}
        </span>
      </button>
      <button onclick={handleClose} class="btn-ghost">
        Done
      </button>
    </div>
  </div>

  <!-- Editor -->
  <div class="flex-1 overflow-auto p-4 b-1 b-solid b-surface-3">
    <textarea
      bind:value={editContent}
      onkeydown={handleKeyDown}
      class="w-full h-full bg-transparent text-text-1 font-mono text-sm resize-none border-none focus:outline-none"
      spellcheck="false"
    ></textarea>
  </div>
</div>
