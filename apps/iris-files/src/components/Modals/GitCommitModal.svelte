<script lang="ts" module>
  /**
   * Modal for creating git commits
   * Shows status (staged, unstaged, untracked files) and allows commit
   */
  import type { CID } from 'hashtree';

  export interface GitCommitTarget {
    dirCid: CID;
    onCommit?: (newCid: CID) => void;
  }

  let show = $state(false);
  let target = $state<GitCommitTarget | null>(null);

  export function open(t: GitCommitTarget) {
    target = t;
    show = true;
  }

  export function close() {
    show = false;
    target = null;
  }
</script>

<script lang="ts">
  import { LinkType } from 'hashtree';
  import { SvelteMap } from 'svelte/reactivity';
  import { createGitStatusStore } from '../../stores/git';
  import { commit } from '../../utils/git';
  import { getErrorMessage } from '../../utils/errorMessage';
  import { getTree } from '../../store';
  import { nostrStore } from '../../nostr';
  import { createProfileStore } from '../../stores/profile';
  import type { GitStatusResult } from '../../utils/wasmGit';

  // Get user info for commit author
  let nostrState = $derived($nostrStore);
  let profileStore = $derived(nostrState.pubkey ? createProfileStore(nostrState.pubkey) : null);
  let profile = $state<{ name?: string; display_name?: string } | undefined>();

  $effect(() => {
    if (!profileStore) {
      profile = undefined;
      return;
    }
    const unsub = profileStore.subscribe(value => {
      profile = value;
    });
    return unsub;
  });

  // Create git status store when modal opens
  let statusStore = $derived(target ? createGitStatusStore(target.dirCid) : null);
  let statusState = $state<{ status: GitStatusResult; loading: boolean; error: string | null }>({
    status: { staged: [], unstaged: [], untracked: [], hasChanges: false },
    loading: true,
    error: null,
  });

  $effect(() => {
    if (!statusStore) {
      statusState = { status: { staged: [], unstaged: [], untracked: [], hasChanges: false }, loading: false, error: null };
      return;
    }
    const unsub = statusStore.subscribe(value => {
      statusState = value;
    });
    return unsub;
  });

  // Form state
  let commitMessage = $state('');
  let isCommitting = $state(false);
  let commitError = $state<string | null>(null);

  // Reset form when modal opens
  $effect(() => {
    if (show) {
      commitMessage = '';
      isCommitting = false;
      commitError = null;
    }
  });

  // Handle ESC key
  function handleKeyDown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      e.preventDefault();
      close();
    }
  }

  $effect(() => {
    if (show) {
      document.addEventListener('keydown', handleKeyDown);
      return () => document.removeEventListener('keydown', handleKeyDown);
    }
  });

  // Get status icon
  function getStatusIcon(status: string): string {
    const x = status[0];
    const y = status[1];
    if (status === '??') return 'i-lucide-file-plus text-success';
    if (x === 'M' || y === 'M') return 'i-lucide-file-edit text-warning';
    if (x === 'A') return 'i-lucide-file-plus text-success';
    if (x === 'D' || y === 'D') return 'i-lucide-file-minus text-error';
    if (x === 'R') return 'i-lucide-file-symlink text-accent';
    return 'i-lucide-file text-text-2';
  }

  // Get status label
  function getStatusLabel(status: string): string {
    const x = status[0];
    const y = status[1];
    if (status === '??') return 'untracked';
    if (x === 'M') return 'modified';
    if (y === 'M') return 'modified';
    if (x === 'A') return 'added';
    if (x === 'D' || y === 'D') return 'deleted';
    if (x === 'R') return 'renamed';
    return status;
  }

  // All files as a flat list with their category
  interface SelectableFile {
    path: string;
    status: string;
    category: 'staged' | 'unstaged' | 'untracked';
    selected: boolean;
  }

  let selectableFiles = $state<SelectableFile[]>([]);

  // Initialize selectable files when status changes
  $effect(() => {
    const files: SelectableFile[] = [];
    for (const f of statusState.status.staged) {
      files.push({ path: f.path, status: f.status, category: 'staged', selected: true });
    }
    for (const f of statusState.status.unstaged) {
      files.push({ path: f.path, status: f.status, category: 'unstaged', selected: true });
    }
    for (const f of statusState.status.untracked) {
      files.push({ path: f.path, status: f.status, category: 'untracked', selected: true });
    }
    selectableFiles = files;
  });

  // Total and selected counts
  let totalChanges = $derived(selectableFiles.length);
  let selectedCount = $derived(selectableFiles.filter(f => f.selected).length);
  let allSelected = $derived(selectableFiles.length > 0 && selectableFiles.every(f => f.selected));

  // Toggle all files
  function toggleAll() {
    const newValue = !allSelected;
    selectableFiles = selectableFiles.map(f => ({ ...f, selected: newValue }));
  }

  // Toggle single file
  function toggleFile(path: string) {
    selectableFiles = selectableFiles.map(f =>
      f.path === path ? { ...f, selected: !f.selected } : f
    );
  }

  // Handle commit
  async function handleCommit() {
    if (!target || !commitMessage.trim() || isCommitting || selectedCount === 0) return;

    isCommitting = true;
    commitError = null;

    try {
      // Get author info
      const authorName = profile?.display_name || profile?.name || 'Anonymous';
      const authorEmail = nostrState.npub ? `${nostrState.npub}@nostr` : 'anonymous@hashtree';

      // Get selected files to commit
      const filesToCommit = selectableFiles.filter(f => f.selected).map(f => f.path);

      // Create commit with selected files
      const result = await commit(target.dirCid, commitMessage.trim(), authorName, authorEmail, filesToCommit);

      if (!result.success) {
        commitError = result.error || 'Failed to create commit';
        return;
      }

      if (!result.gitFiles) {
        commitError = 'No git files returned';
        return;
      }

      // Build new directory with updated .git
      const tree = getTree();
      const entries = await tree.listDirectory(target.dirCid);

      // Build entries for the new directory, replacing .git
      const newEntries: Array<{ name: string; cid: CID; size: number; type: LinkType }> = [];

      // Add non-.git entries from original
      for (const entry of entries) {
        if (entry.name !== '.git') {
          newEntries.push({
            name: entry.name,
            cid: entry.cid,
            size: entry.size,
            type: entry.type,
          });
        }
      }

      // Build .git directory from commit result
      const gitDirMap = new SvelteMap<string, Array<{ name: string; cid: CID; size: number; type: LinkType }>>();
      gitDirMap.set('.git', []);

      // Create directory entries for subdirectories
      for (const file of result.gitFiles) {
        if (file.isDir && file.name.startsWith('.git/')) {
          gitDirMap.set(file.name, []);
        }
      }

      // Process files
      for (const file of result.gitFiles) {
        if (!file.isDir && file.name.startsWith('.git/')) {
          const { cid, size } = await tree.putFile(file.data);
          const parentDir = file.name.substring(0, file.name.lastIndexOf('/'));
          const fileName = file.name.substring(file.name.lastIndexOf('/') + 1);

          const parentEntries = gitDirMap.get(parentDir);
          if (parentEntries) {
            parentEntries.push({ name: fileName, cid, size, type: LinkType.Blob });
          }
        }
      }

      // Build directories from deepest to root
      const sortedDirs = Array.from(gitDirMap.keys())
        .filter(d => d !== '.git')
        .sort((a, b) => b.split('/').length - a.split('/').length);

      for (const dirPath of sortedDirs) {
        const dirEntries = gitDirMap.get(dirPath) || [];
        const { cid } = await tree.putDirectory(dirEntries);

        const parentDir = dirPath.substring(0, dirPath.lastIndexOf('/'));
        const dirName = dirPath.substring(dirPath.lastIndexOf('/') + 1);

        const parentEntries = gitDirMap.get(parentDir);
        if (parentEntries) {
          parentEntries.push({ name: dirName, cid, size: 0, type: LinkType.Dir });
        }
      }

      // Build .git directory
      const gitEntries = gitDirMap.get('.git') || [];
      const { cid: gitCid } = await tree.putDirectory(gitEntries);

      newEntries.push({ name: '.git', cid: gitCid, size: 0, type: LinkType.Dir });

      // Build new root directory
      const { cid: newDirCid } = await tree.putDirectory(newEntries);

      // Call onCommit callback
      if (target.onCommit) {
        await target.onCommit(newDirCid);
      }

      close();
    } catch (err) {
      commitError = getErrorMessage(err);
    } finally {
      isCommitting = false;
    }
  }

</script>

{#if show && target}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div class="fixed inset-0 z-50 flex items-center justify-center bg-black/50" onclick={close}>
    <div class="bg-surface-1 rounded-lg shadow-lg w-full max-w-xl mx-4 max-h-[80vh] flex flex-col" onclick={(e) => e.stopPropagation()}>
      <!-- Header -->
      <div class="flex items-center justify-between p-4 b-b-1 b-b-solid b-b-surface-3">
        <h2 class="text-lg font-semibold flex items-center gap-2">
          <span class="i-lucide-git-commit"></span>
          Commit Changes
        </h2>
        <button onclick={close} class="btn-ghost p-1" aria-label="Close">
          <span class="i-lucide-x text-lg"></span>
        </button>
      </div>

      <!-- Content -->
      <div class="flex-1 overflow-auto p-4">
        {#if statusState.loading}
          <div class="flex items-center justify-center py-8 text-text-3">
            <span class="i-lucide-loader-2 animate-spin mr-2"></span>
            Loading changes...
          </div>
        {:else if statusState.error}
          <div class="flex items-center justify-center py-8 text-error">
            <span class="i-lucide-alert-circle mr-2"></span>
            {statusState.error}
          </div>
        {:else if totalChanges === 0}
          <div class="flex items-center justify-center py-8 text-text-3">
            <span class="i-lucide-check-circle mr-2 text-success"></span>
            No changes to commit
          </div>
        {:else}
          {#if commitError}
            <div class="mb-4 p-3 bg-error/10 text-error rounded-lg text-sm flex items-center gap-2">
              <span class="i-lucide-alert-circle"></span>
              {commitError}
            </div>
          {/if}

          <!-- Commit message input -->
          <div class="mb-4">
            <label for="commit-message" class="block text-sm font-medium text-text-2 mb-1">
              Commit message
            </label>
            <textarea
              id="commit-message"
              bind:value={commitMessage}
              placeholder="Describe your changes..."
              class="w-full px-3 py-2 bg-surface-0 b-1 b-solid b-surface-3 rounded-lg text-sm resize-none focus:outline-none focus:b-accent"
              rows="3"
            ></textarea>
          </div>

          <!-- Select all toggle -->
          <div class="flex items-center gap-2 mb-3">
            <button
              onclick={toggleAll}
              class="btn-ghost text-xs px-2 py-1 flex items-center gap-1"
            >
              {#if allSelected}
                <span class="i-lucide-check-square text-accent"></span>
                Deselect all
              {:else}
                <span class="i-lucide-square text-text-3"></span>
                Select all
              {/if}
            </button>
            <span class="text-xs text-text-3">
              {selectedCount} of {totalChanges} selected
            </span>
          </div>

          <!-- Changes list with checkboxes -->
          <div class="bg-surface-0 rounded-lg b-1 b-solid b-surface-3 overflow-hidden max-h-64 overflow-y-auto">
            {#each selectableFiles as file (file.path)}
              {@const isSelected = file.selected}
              <button
                onclick={() => toggleFile(file.path)}
                class="w-full flex items-center gap-2 px-3 py-2 text-sm b-b-1 b-b-solid b-b-surface-3 last:b-b-0 hover:bg-surface-1 text-left b-0 bg-transparent cursor-pointer"
              >
                <!-- Checkbox -->
                <span class={isSelected ? 'i-lucide-check-square text-accent' : 'i-lucide-square text-text-3'}></span>
                <!-- Status icon -->
                <span class={getStatusIcon(file.status)}></span>
                <!-- File path -->
                <span class="truncate flex-1 {isSelected ? 'text-text-1' : 'text-text-3'}">{file.path}</span>
                <!-- Status label -->
                <span class="text-xs text-text-3 shrink-0">
                  {#if file.category === 'untracked'}
                    new
                  {:else}
                    {getStatusLabel(file.status)}
                  {/if}
                </span>
              </button>
            {/each}
          </div>

          <p class="mt-3 text-xs text-text-3">
            {#if selectedCount === totalChanges}
              All changes will be committed.
            {:else if selectedCount === 0}
              No files selected.
            {:else}
              {selectedCount} file{selectedCount !== 1 ? 's' : ''} will be committed. {totalChanges - selectedCount} file{totalChanges - selectedCount !== 1 ? 's' : ''} will remain uncommitted.
            {/if}
          </p>
        {/if}
      </div>

      <!-- Footer -->
      <div class="flex justify-between items-center p-4 b-t-1 b-t-solid b-t-surface-3">
        <span class="text-sm text-text-3">
          {selectedCount} of {totalChanges} file{totalChanges !== 1 ? 's' : ''} selected
        </span>
        <div class="flex gap-2">
          <button onclick={close} class="btn-ghost">Cancel</button>
          <button
            onclick={handleCommit}
            disabled={isCommitting || selectedCount === 0 || !commitMessage.trim()}
            class="btn-primary flex items-center gap-1"
          >
            {#if isCommitting}
              <span class="i-lucide-loader-2 animate-spin"></span>
              Committing...
            {:else}
              <span class="i-lucide-git-commit"></span>
              Commit {selectedCount} file{selectedCount !== 1 ? 's' : ''}
            {/if}
          </button>
        </div>
      </div>
    </div>
  </div>
{/if}
