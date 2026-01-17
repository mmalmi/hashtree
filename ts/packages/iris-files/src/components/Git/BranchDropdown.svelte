<script lang="ts">
  /**
   * BranchDropdown - Git branch selector with new branch creation and comparison
   */
  import type { CID } from 'hashtree';
  import { LinkType } from 'hashtree';
  import { createBranch, applyGitChanges } from '../../utils/git';
  import { getErrorMessage } from '../../utils/errorMessage';
  import { navigate } from '../../lib/router.svelte';
  import { routeStore } from '../../stores';
  import { autosaveIfOwn } from '../../nostr';
  import { getTree } from '../../store';
  import Dropdown from '../ui/Dropdown.svelte';

  interface Props {
    branches: string[];
    currentBranch: string | null;
    branchDisplay: string;
    canEdit: boolean;
    dirCid: CID;
    npub?: string;
    /** Full path to git repo (treeName + subdirectory path if any) */
    repoPath?: string;
    onBranchSelect?: (branch: string) => void;
    /** Show loading spinner (e.g., when switching branches) */
    loading?: boolean;
  }

  let { branches, currentBranch, branchDisplay, canEdit, dirCid, npub, repoPath, onBranchSelect, loading = false }: Props = $props();

  let route = $derived($routeStore);

  // Branch dropdown state
  let isDropdownOpen = $state(false);
  let showNewBranchInput = $state(false);
  let showCompareSelect = $state(false);
  let newBranchName = $state('');
  let isCreatingBranch = $state(false);
  let branchError = $state<string | null>(null);

  function handleBranchSelect(branch: string) {
    if (onBranchSelect) {
      onBranchSelect(branch);
    }
    isDropdownOpen = false;
  }

  // Navigate to compare view
  function handleCompareSelect(targetBranch: string) {
    if (!currentBranch || !npub || !repoPath) return;
    const linkKeySuffix = route.params.get('k') ? `&k=${route.params.get('k')}` : '';
    navigate(`/${npub}/${repoPath}?compare=${currentBranch}...${targetBranch}${linkKeySuffix}`);
    isDropdownOpen = false;
    showCompareSelect = false;
  }

  // Handle new branch creation
  async function handleCreateBranch() {
    if (!newBranchName.trim() || isCreatingBranch) return;

    isCreatingBranch = true;
    branchError = null;

    try {
      const result = await createBranch(dirCid, newBranchName.trim(), true);
      if (!result.success) {
        branchError = result.error || 'Failed to create branch';
        return;
      }

      // Persist the updated .git files
      if (result.gitFiles) {
        const newDirCid = await applyGitChanges(dirCid, result.gitFiles);

        // Get current tree root and update it with the new directory
        const { getCurrentRootCid } = await import('../../actions/route');
        const currentPath = route.path;
        const treeRootCid = getCurrentRootCid();

        if (treeRootCid) {
          let newRootCid;
          if (currentPath.length === 0) {
            // Git repo is at tree root
            newRootCid = newDirCid;
          } else {
            // Git repo is in a subdirectory - replace it at that path
            const tree = getTree();
            const parentPath = currentPath.slice(0, -1);
            const dirName = currentPath[currentPath.length - 1];
            newRootCid = await tree.setEntry(
              treeRootCid,
              parentPath,
              dirName,
              newDirCid,
              0,
              LinkType.Dir
            );
          }

          // Save and publish - UI will react automatically via store subscriptions
          autosaveIfOwn(newRootCid);
        }
      }

      // Success - close dropdown and reset
      showNewBranchInput = false;
      newBranchName = '';
      isDropdownOpen = false;
    } catch (err) {
      branchError = getErrorMessage(err);
    } finally {
      isCreatingBranch = false;
    }
  }

  function handleClose() {
    isDropdownOpen = false;
    showNewBranchInput = false;
    showCompareSelect = false;
    branchError = null;
  }
</script>

<Dropdown bind:open={isDropdownOpen} onClose={handleClose}>
  {#snippet trigger()}
    <button
      onclick={() => isDropdownOpen = !isDropdownOpen}
      class="btn-ghost flex items-center gap-1 px-3 h-9 text-sm"
      disabled={loading}
    >
      {#if loading}
        <span class="i-lucide-loader-2 animate-spin"></span>
      {:else}
        <span class="i-lucide-git-branch"></span>
      {/if}
      <span class={currentBranch ? '' : 'font-mono text-xs'}>{branchDisplay}</span>
      {#if !loading}
        <span class="i-lucide-chevron-down text-xs"></span>
      {/if}
    </button>
  {/snippet}
  <!-- Branch list -->
  {#each branches as branch (branch)}
    <button
      onclick={() => handleBranchSelect(branch)}
      class="w-full text-left px-3 py-1.5 text-sm bg-surface-2 hover:bg-surface-3 flex items-center gap-2 text-text-1 b-0 cursor-pointer"
    >
      {#if branch === currentBranch}
        <span class="i-lucide-check text-accent text-xs"></span>
      {:else}
        <span class="ml-4"></span>
      {/if}
      <span>{branch}</span>
    </button>
  {/each}
  <!-- Compare branches option (when there are multiple branches and we have navigation info) -->
  {#if branches.length > 1 && npub && repoPath && currentBranch}
    {#if showCompareSelect}
      <div class="px-3 py-2 bg-surface-2 b-t-1 b-t-solid b-t-surface-3">
        <div class="text-xs text-text-3 mb-2">Compare {currentBranch} with:</div>
        {#each branches.filter(b => b !== currentBranch) as branch (branch)}
          <button
            onclick={() => handleCompareSelect(branch)}
            class="w-full text-left px-2 py-1.5 text-sm bg-surface-2 hover:bg-surface-3 flex items-center gap-2 text-text-1 b-0 rounded mb-1 cursor-pointer"
          >
            <span class="i-lucide-git-branch text-xs text-accent"></span>
            <span>{branch}</span>
          </button>
        {/each}
        <button
          onclick={() => { showCompareSelect = false; }}
          class="btn-ghost text-xs px-2 py-1 mt-1"
        >
          Cancel
        </button>
      </div>
    {:else}
      <button
        onclick={() => { showCompareSelect = true; }}
        class="w-full text-left px-3 py-1.5 text-sm bg-surface-2 hover:bg-surface-3 flex items-center gap-2 text-text-1 b-0 b-t-1 b-t-solid b-t-surface-3 cursor-pointer"
      >
        <span class="i-lucide-git-compare text-xs"></span>
        <span>Compare branches</span>
      </button>
    {/if}
  {/if}
  <!-- New branch option (only for editors) -->
  {#if canEdit}
    {#if showNewBranchInput}
      <div class="px-3 py-2 bg-surface-2 b-t-1 b-t-solid b-t-surface-3">
        {#if branchError}
          <div class="text-xs text-error mb-2">{branchError}</div>
        {/if}
        <input
          type="text"
          bind:value={newBranchName}
          placeholder="Branch name"
          class="w-full px-2 py-1 text-sm bg-surface-0 b-1 b-solid b-surface-3 rounded focus:outline-none focus:b-accent"
          onkeydown={(e) => { if (e.key === 'Enter') handleCreateBranch(); if (e.key === 'Escape') { showNewBranchInput = false; branchError = null; } }}
        />
        <div class="flex gap-2 mt-2">
          <button
            onclick={handleCreateBranch}
            disabled={isCreatingBranch || !newBranchName.trim()}
            class="btn-primary text-xs px-2 py-1 flex items-center gap-1"
          >
            {#if isCreatingBranch}
              <span class="i-lucide-loader-2 animate-spin"></span>
            {:else}
              <span class="i-lucide-plus"></span>
            {/if}
            Create
          </button>
          <button
            onclick={() => { showNewBranchInput = false; branchError = null; }}
            class="btn-ghost text-xs px-2 py-1"
          >
            Cancel
          </button>
        </div>
      </div>
    {:else}
      <button
        onclick={() => { showNewBranchInput = true; newBranchName = ''; }}
        class="w-full text-left px-3 py-1.5 text-sm bg-surface-2 hover:bg-surface-3 flex items-center gap-2 text-text-1 b-0 b-t-1 b-t-solid b-t-surface-3 cursor-pointer"
      >
        <span class="i-lucide-git-branch-plus text-xs"></span>
        <span>New branch</span>
      </button>
    {/if}
  {/if}
</Dropdown>
