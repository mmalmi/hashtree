<script lang="ts" module>
  /**
   * Modal for creating new files, folders, or trees
   */
  import type { TreeVisibility } from 'hashtree';

  export type ModalType = 'file' | 'folder' | 'tree' | 'document';

  let show = $state(false);
  let modalType = $state<ModalType>('file');
  let treeVisibility = $state<TreeVisibility>('public');
  let modalInput = $state('');

  export function open(type: ModalType, visibility: TreeVisibility = 'public') {
    modalType = type;
    treeVisibility = visibility;
    modalInput = '';
    show = true;
  }

  export function close() {
    show = false;
    modalInput = '';
  }

  export function setVisibility(v: TreeVisibility) {
    treeVisibility = v;
  }
</script>

<script lang="ts">
  import { createFile, createFolder, createTree, createDocument } from '../../actions';
  import { routeStore } from '../../stores';
  import { navigate } from '../../lib/router.svelte';
  import VisibilityPicker from './VisibilityPicker.svelte';

  let route = $derived($routeStore);

  let isCreating = $state(false);
  let inputRef = $state<HTMLInputElement | null>(null);

  // Focus input when modal opens
  $effect(() => {
    if (show && inputRef) {
      inputRef.focus();
    }
  });

  let isFolder = $derived(modalType === 'folder');
  let isTree = $derived(modalType === 'tree');
  let isDocument = $derived(modalType === 'document');

  let title = $derived(
    isTree ? 'New Folder' : isDocument ? 'New Document' : isFolder ? 'New Folder' : 'New File'
  );
  let placeholder = $derived(
    isDocument ? 'Document name...' : (isTree || isFolder ? 'Folder name...' : 'File name...')
  );

  async function handleSubmit(e?: Event) {
    e?.preventDefault();
    const name = modalInput.trim();
    if (!name || isCreating) return;

    if (isTree) {
      isCreating = true;
      await createTree(name, treeVisibility);
      isCreating = false;
      close();
    } else if (isDocument) {
      isCreating = true;
      if (route.npub && route.treeName) {
        // Add document to existing tree
        await createDocument(name);
        close();
        const newPath = [...route.path, name].map(encodeURIComponent).join('/');
        const linkKeyParam = route.params.get('k') ? `?k=${route.params.get('k')}` : '';
        window.location.hash = `/${route.npub}/${route.treeName}/${newPath}${linkKeyParam}`;
      } else {
        // Create new tree as a document (from docs home)
        const { createDocumentTree } = await import('../../actions/tree');
        const result = await createDocumentTree(name, treeVisibility);
        close();
        if (result.npub && result.treeName) {
          const linkKeyParam = result.linkKey ? `?k=${result.linkKey}` : '';
          navigate(`/${result.npub}/${encodeURIComponent(result.treeName)}${linkKeyParam}`);
        }
      }
      isCreating = false;
    } else if (isFolder) {
      createFolder(name);
      close();
    } else {
      createFile(name, '');
      close();
    }
  }

  function handleKeyDown(e: KeyboardEvent) {
    if (e.key === 'Escape') close();
    if (e.key === 'Enter') handleSubmit();
  }
</script>

{#if show}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/70"
    onclick={close}
  >
    <div
      class="bg-surface-1 rounded-lg shadow-lg p-6 w-full max-w-md mx-4"
      onclick={(e) => e.stopPropagation()}
      onkeydown={handleKeyDown}
    >
      <h2 class="text-lg font-semibold mb-4">{title}</h2>
      <form onsubmit={(e) => { e.preventDefault(); handleSubmit(); }}>
        <input
          bind:this={inputRef}
          type="text"
          bind:value={modalInput}
          placeholder={placeholder}
          class="input w-full mb-4"
        />

        <!-- Visibility picker for trees and new documents -->
        {#if isTree || (isDocument && !route.treeName)}
          <div class="mt-4 mb-4">
            <VisibilityPicker value={treeVisibility} onchange={setVisibility} />
          </div>
        {/if}

        <div class="flex justify-end gap-2">
          <button type="button" onclick={close} class="btn-ghost">Cancel</button>
          <button type="submit" class="btn-success" disabled={isCreating}>
            {isCreating ? 'Creating...' : 'Create'}
          </button>
        </div>
      </form>
    </div>
  </div>
{/if}
