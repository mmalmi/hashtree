<script lang="ts" module>
  /**
   * Modal for creating a new pull request
   * Supports:
   * - Branch selection via dropdowns when branches are available
   * - Cross-repo PRs with source repo specification (npub/path or nhash)
   */

  export interface NewPullRequestTarget {
    npub: string;
    repoName: string;
    branches?: string[];
    currentBranch?: string;
    onCreate?: (pr: { id: string; title: string }) => void;
  }

  let isOpen = $state(false);
  let target = $state<NewPullRequestTarget | null>(null);

  export function open(t: NewPullRequestTarget) {
    target = t;
    isOpen = true;
  }

  export function close() {
    isOpen = false;
    target = null;
  }
</script>

<script lang="ts">
  import { createPullRequest } from '../../nip34';
  import { getWorkerAdapter } from '../../lib/workerInit';
  import { createGitInfoStore } from '../../stores/git';
  import { getLocalRootCache, getLocalRootKey } from '../../treeRootCache';
  import { waitForTreeRoot } from '../../stores/treeRoot';
  import type { CID } from 'hashtree';

  // Available branches from the target (destination) repo
  let branches = $derived(target?.branches || []);
  let defaultBranch = $derived(branches.includes('main') ? 'main' : branches.includes('master') ? 'master' : branches[0] || 'main');

  let title = $state('');
  let description = $state('');

  // Source (head) - where the changes come from
  let sourceBranch = $state('');
  let sourceRepo = $state(''); // Optional: npub/path or nhash for cross-repo PRs

  // Fork state
  let forkRootCid = $state<CID | null>(null);
  let forkBranches = $state<string[]>([]);
  let forkLoading = $state(false);
  let forkError = $state<string | null>(null);
  let forkResolveToken = 0;

  // Parse fork URL to get npub/repoName
  function parseForkUrl(url: string): { npub: string; repoName: string } | null {
    const trimmed = url.trim();
    if (!trimmed) return null;

    // Handle htree:// URLs
    if (trimmed.startsWith('htree://')) {
      const path = trimmed.slice(8);
      const parts = path.split('/').filter(Boolean);
      if (parts.length >= 2 && parts[0].startsWith('npub1')) {
        return { npub: parts[0], repoName: parts.slice(1).join('/') };
      }
    }

    // Handle npub/repo format
    const parts = trimmed.split('/').filter(Boolean);
    if (parts.length >= 2 && parts[0].startsWith('npub1')) {
      return { npub: parts[0], repoName: parts.slice(1).join('/') };
    }

    return null;
  }

  // Resolve fork root when URL changes
  $effect(() => {
    if (!showSourceRepo || !sourceRepo.trim()) {
      forkRootCid = null;
      forkBranches = [];
      forkError = null;
      return;
    }

    const parsed = parseForkUrl(sourceRepo);
    if (!parsed) {
      forkRootCid = null;
      forkBranches = [];
      forkError = null;
      return;
    }

    forkLoading = true;
    forkError = null;
    const token = ++forkResolveToken;

    const resolveFork = async (): Promise<CID | null> => {
      const adapter = getWorkerAdapter();
      if (adapter) {
        try {
          const cid = await adapter.resolveRoot(parsed.npub, parsed.repoName);
          if (cid) return cid;
        } catch (err) {
          const message = err instanceof Error ? err.message : 'Failed to resolve fork';
          if (token === forkResolveToken) {
            forkError = message;
          }
        }
      }

      const localHash = getLocalRootCache(parsed.npub, parsed.repoName);
      if (localHash) {
        return { hash: localHash, key: getLocalRootKey(parsed.npub, parsed.repoName) };
      }

      return waitForTreeRoot(parsed.npub, parsed.repoName, 7000);
    };

    resolveFork().then((cid) => {
      if (token !== forkResolveToken) return;
      forkRootCid = cid;
      forkLoading = false;
      if (!cid) {
        forkError = forkError ?? 'Could not resolve fork';
      }
    });
  });

  // Create git info store for fork when we have its root CID
  let forkGitStore = $derived(forkRootCid ? createGitInfoStore(forkRootCid) : null);

  // Subscribe to fork git info
  $effect(() => {
    if (!forkGitStore) {
      forkBranches = [];
      return;
    }
    const unsub = forkGitStore.subscribe(info => {
      forkBranches = info.branches;
      // Clear source branch if it doesn't exist in new fork branches
      if (sourceBranch && info.branches.length > 0 && !info.branches.includes(sourceBranch)) {
        sourceBranch = '';
      }
    });
    return unsub;
  });

  // Source branches: use fork branches if available, otherwise target repo branches
  let sourceBranchOptions = $derived(showSourceRepo && forkBranches.length > 0 ? forkBranches : branches);

  // Show source repo field by default if user isn't the repo owner
  import { nostrStore } from '../../nostr';
  let userNpub = $derived($nostrStore.npub);
  let isOwner = $derived(target?.npub === userNpub);
  let showSourceRepo = $state(false);

  // Target (base) - where to merge into
  let targetBranch = $state('');

  let isSubmitting = $state(false);
  let error = $state<string | null>(null);

  // Dropdown open states
  let sourceBranchDropdownOpen = $state(false);
  let targetBranchDropdownOpen = $state(false);

  // Initialize when modal opens
  $effect(() => {
    if (isOpen && target) {
      // Pre-select current branch as source if it's not the default
      if (target.currentBranch && target.currentBranch !== defaultBranch) {
        sourceBranch = target.currentBranch;
      } else if (branches.length > 1) {
        // Find first branch that isn't the default
        const nonDefault = branches.find(b => b !== defaultBranch);
        sourceBranch = nonDefault || '';
      }
      targetBranch = defaultBranch;
      sourceRepo = '';
      // Show source repo by default if user isn't the owner (contributing from fork)
      showSourceRepo = !isOwner;
    }
  });

  function handleClose() {
    title = '';
    description = '';
    sourceBranch = '';
    sourceRepo = '';
    showSourceRepo = false;
    targetBranch = '';
    error = null;
    close();
  }

  async function handleSubmit(e: Event) {
    e.preventDefault();
    if (!target || !title.trim()) return;

    // Validate branch selection
    if (!sourceBranch.trim()) {
      error = 'Please select or enter a source branch';
      return;
    }

    isSubmitting = true;
    error = null;

    // Capture target values before async call (handleClose sets target to null)
    const targetNpub = target.npub;
    const targetRepoName = target.repoName;
    const onCreate = target.onCreate;

    try {
      const pr = await createPullRequest(targetNpub, targetRepoName, title.trim(), description.trim(), {
        branch: sourceBranch.trim(),
        targetBranch: targetBranch.trim() || 'main',
        // Include source repo info in clone URL if specified
        cloneUrl: sourceRepo.trim() || undefined,
      });

      if (pr) {
        onCreate?.({ id: pr.id, title: pr.title });
        handleClose();
        // Navigate to the new PR using query params
        window.location.hash = `/${targetNpub}/${targetRepoName}?tab=pulls&id=${pr.id}`;
      } else {
        error = 'Failed to create pull request';
      }
    } catch (err) {
      error = err instanceof Error ? err.message : 'Failed to create pull request';
    } finally {
      isSubmitting = false;
    }
  }

  function handleKeyDown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      handleClose();
    }
  }

  function selectSourceBranch(branch: string) {
    sourceBranch = branch;
    sourceBranchDropdownOpen = false;
  }

  function selectTargetBranch(branch: string) {
    targetBranch = branch;
    targetBranchDropdownOpen = false;
  }
</script>

{#if isOpen && target}
  <div
    class="fixed inset-0 bg-black/50 flex items-center justify-center z-50"
    role="dialog" tabindex="-1"
    aria-modal="true"
    onclick={(e) => e.target === e.currentTarget && handleClose()}
    onkeydown={handleKeyDown}
  >
    <div class="bg-surface-0 rounded-lg shadow-xl w-full max-w-lg mx-4 overflow-hidden">
      <!-- Header -->
      <div class="flex items-center justify-between px-4 py-3 b-b-1 b-b-solid b-b-surface-3">
        <h2 class="text-lg font-semibold">New Pull Request</h2>
        <button onclick={handleClose} class="btn-ghost p-1" aria-label="Close">
          <span class="i-lucide-x text-lg"></span>
        </button>
      </div>

      <!-- Form -->
      <form onsubmit={handleSubmit} class="p-4 flex flex-col gap-4">
        {#if error}
          <div class="px-3 py-2 bg-danger/10 text-danger rounded-md text-sm">
            {error}
          </div>
        {/if}

        <!-- Title -->
        <div class="flex flex-col gap-1.5">
          <label for="pr-title" class="text-sm font-medium">Title</label>
          <input
            id="pr-title"
            type="text"
            bind:value={title}
            placeholder="Brief description of changes"
            class="px-3 py-2 bg-surface-1 b-1 b-solid b-surface-3 rounded-md focus:outline-none focus:b-accent"
            required
          />
        </div>

        <!-- Source selection: this repo or fork -->
        <div class="flex flex-col gap-3">
          <div class="text-sm font-medium">Source</div>

          <!-- Source type toggle -->
          <div class="flex gap-2">
            <button
              type="button"
              onclick={() => showSourceRepo = false}
              class="flex-1 px-3 py-2 rounded-md text-sm flex items-center justify-center gap-2 b-1 b-solid transition-colors {!showSourceRepo ? 'bg-accent/10 b-accent text-accent' : 'bg-surface-1 b-surface-3 text-text-2 hover:bg-surface-2'}"
            >
              <span class="i-lucide-git-branch"></span>
              This repository
            </button>
            <button
              type="button"
              onclick={() => showSourceRepo = true}
              class="flex-1 px-3 py-2 rounded-md text-sm flex items-center justify-center gap-2 b-1 b-solid transition-colors {showSourceRepo ? 'bg-accent/10 b-accent text-accent' : 'bg-surface-1 b-surface-3 text-text-2 hover:bg-surface-2'}"
            >
              <span class="i-lucide-git-fork"></span>
              From fork
            </button>
          </div>

          <!-- Source repo input (for cross-repo PRs) -->
          {#if showSourceRepo}
            <div class="flex flex-col gap-1.5">
              <label for="pr-source-repo" class="text-xs text-text-3">Fork URL</label>
              <input
                id="pr-source-repo"
                type="text"
                bind:value={sourceRepo}
                placeholder="npub.../repo or htree://..."
                class="px-3 py-2 bg-surface-1 b-1 b-solid b-surface-3 rounded-md focus:outline-none focus:b-accent text-sm font-mono"
              />
            </div>
          {/if}
        </div>

        <!-- Branch selection -->
        <div class="flex flex-col gap-3">
          <div class="text-sm font-medium">Branches</div>

          <div class="flex items-center gap-2">
            <!-- Source (head) branch -->
            <div class="flex-1">
              <div class="text-xs text-text-3 mb-1">From{#if showSourceRepo && sourceRepo.trim()} (fork){/if}</div>
              {#if showSourceRepo && forkLoading}
                <div class="w-full px-3 py-2 bg-surface-1 b-1 b-solid b-surface-3 rounded-md text-sm flex items-center gap-2 text-text-3">
                  <span class="i-lucide-loader-2 animate-spin"></span>
                  Loading branches...
                </div>
              {:else if showSourceRepo && forkError}
                <div class="w-full px-3 py-2 bg-surface-1 b-1 b-solid b-danger/50 rounded-md text-sm text-danger">
                  {forkError}
                </div>
              {:else if sourceBranchOptions.length > 0}
                <div class="relative">
                  <button
                    type="button"
                    onclick={() => sourceBranchDropdownOpen = !sourceBranchDropdownOpen}
                    class="w-full px-3 py-2 bg-surface-1 b-1 b-solid b-surface-3 rounded-md text-left flex items-center justify-between text-sm"
                  >
                    <span class="flex items-center gap-2">
                      <span class="i-lucide-git-branch text-accent"></span>
                      <span class="font-mono">{sourceBranch || 'Select branch'}</span>
                    </span>
                    <span class="i-lucide-chevron-down text-text-3"></span>
                  </button>
                  {#if sourceBranchDropdownOpen}
                    <div class="absolute top-full left-0 right-0 mt-1 bg-surface-1 b-1 b-solid b-surface-3 rounded-md shadow-lg z-10 max-h-48 overflow-auto">
                      {#each sourceBranchOptions as branch (branch)}
                        <button
                          type="button"
                          onclick={() => selectSourceBranch(branch)}
                          class="w-full px-3 py-2 text-left text-sm hover:bg-surface-2 flex items-center gap-2 b-0 bg-transparent cursor-pointer"
                        >
                          {#if branch === sourceBranch}
                            <span class="i-lucide-check text-accent"></span>
                          {:else}
                            <span class="w-4"></span>
                          {/if}
                          <span class="font-mono">{branch}</span>
                        </button>
                      {/each}
                    </div>
                  {/if}
                </div>
              {:else}
                <input
                  type="text"
                  bind:value={sourceBranch}
                  placeholder="feature/..."
                  class="w-full px-3 py-2 bg-surface-1 b-1 b-solid b-surface-3 rounded-md focus:outline-none focus:b-accent text-sm font-mono"
                />
              {/if}
            </div>

            <!-- Arrow -->
            <div class="pt-5">
              <span class="i-lucide-arrow-right text-text-3"></span>
            </div>

            <!-- Target (base) branch -->
            <div class="flex-1">
              <div class="text-xs text-text-3 mb-1">Into (target)</div>
              {#if branches.length > 0}
                <div class="relative">
                  <button
                    type="button"
                    onclick={() => targetBranchDropdownOpen = !targetBranchDropdownOpen}
                    class="w-full px-3 py-2 bg-surface-1 b-1 b-solid b-surface-3 rounded-md text-left flex items-center justify-between text-sm"
                  >
                    <span class="flex items-center gap-2">
                      <span class="i-lucide-git-branch"></span>
                      <span class="font-mono">{targetBranch || 'Select branch'}</span>
                    </span>
                    <span class="i-lucide-chevron-down text-text-3"></span>
                  </button>
                  {#if targetBranchDropdownOpen}
                    <div class="absolute top-full left-0 right-0 mt-1 bg-surface-1 b-1 b-solid b-surface-3 rounded-md shadow-lg z-10 max-h-48 overflow-auto">
                      {#each branches as branch (branch)}
                        <button
                          type="button"
                          onclick={() => selectTargetBranch(branch)}
                          class="w-full px-3 py-2 text-left text-sm hover:bg-surface-2 flex items-center gap-2 b-0 bg-transparent cursor-pointer"
                        >
                          {#if branch === targetBranch}
                            <span class="i-lucide-check text-accent"></span>
                          {:else}
                            <span class="w-4"></span>
                          {/if}
                          <span class="font-mono">{branch}</span>
                        </button>
                      {/each}
                    </div>
                  {/if}
                </div>
              {:else}
                <input
                  type="text"
                  bind:value={targetBranch}
                  placeholder="main"
                  class="w-full px-3 py-2 bg-surface-1 b-1 b-solid b-surface-3 rounded-md focus:outline-none focus:b-accent text-sm font-mono"
                />
              {/if}
            </div>
          </div>

        </div>

        <!-- Description -->
        <div class="flex flex-col gap-1.5">
          <label for="pr-description" class="text-sm font-medium">Description</label>
          <textarea
            id="pr-description"
            bind:value={description}
            placeholder="Detailed explanation of the changes (markdown supported)"
            rows="4"
            class="px-3 py-2 bg-surface-1 b-1 b-solid b-surface-3 rounded-md focus:outline-none focus:b-accent resize-y"
          ></textarea>
        </div>

        <!-- Actions -->
        <div class="flex justify-end gap-2 pt-2">
          <button type="button" onclick={handleClose} class="btn-ghost px-4 py-2">
            Cancel
          </button>
          <button
            type="submit"
            disabled={!title.trim() || !sourceBranch.trim() || isSubmitting}
            class="btn-primary px-4 py-2 flex items-center gap-2"
          >
            {#if isSubmitting}
              <span class="i-lucide-loader-2 animate-spin"></span>
            {:else}
              <span class="i-lucide-git-pull-request"></span>
            {/if}
            Create Pull Request
          </button>
        </div>
      </form>
    </div>
  </div>
{/if}
