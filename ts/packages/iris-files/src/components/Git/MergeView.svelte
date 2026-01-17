<script lang="ts">
  /**
   * MergeView - UI for performing a merge between branches
   * Shows merge preview, commit message input, and confirmation
   * Optionally updates PR status after merge if prEventId/prAuthorPubkey are provided
   */
  import { diffBranches, canMerge, mergeBranches, applyGitChanges } from '../../utils/git';
  import { getErrorMessage } from '../../utils/errorMessage';
  import { routeStore, treeRootStore, createTreesStore, currentDirCidStore } from '../../stores';
  import { nostrStore, autosaveIfOwn } from '../../nostr';
  import { updateStatus } from '../../nip34';
  import FileBrowser from '../FileBrowser.svelte';
  import ViewerHeader from '../Viewer/ViewerHeader.svelte';
  import RepoTabNav from './RepoTabNav.svelte';

  interface Props {
    npub: string;
    repoName: string;
    baseBranch: string;
    headBranch: string;
    /** PR event ID - if provided, PR status will be updated to 'merged' after successful merge */
    prEventId?: string;
    /** PR author pubkey - required if prEventId is provided */
    prAuthorPubkey?: string;
  }

  let { npub, repoName, baseBranch, headBranch, prEventId, prAuthorPubkey }: Props = $props();

  let route = $derived($routeStore);
  let rootCid = $derived($treeRootStore);
  let dirCid = $derived($currentDirCidStore);

  // Get tree visibility info
  let treesStore = $derived(createTreesStore(npub));
  let trees = $state<Array<{ name: string; visibility?: string }>>([]);

  $effect(() => {
    const store = treesStore;
    const unsub = store.subscribe(value => {
      trees = value;
    });
    return unsub;
  });

  // Extract the base tree name from repoName
  let baseTreeName = $derived(repoName.split('/')[0]);
  let currentTree = $derived(trees.find(t => t.name === baseTreeName));

  // Build back URL (to compare view) - use repoName which includes full path to git repo
  let backUrl = $derived.by(() => {
    const linkKeySuffix = route.params.get('k') ? `&k=${route.params.get('k')}` : '';
    return `#/${npub}/${repoName}?compare=${baseBranch}...${headBranch}${linkKeySuffix}`;
  });

  // Check if current user can merge (owns the repo)
  let nostr = $derived($nostrStore);
  let canMergeCheck = $derived(nostr?.npub === npub);

  // State
  let loading = $state(true);
  let merging = $state(false);
  let error = $state<string | null>(null);
  let mergeError = $state<string | null>(null);
  let mergeSuccess = $state(false);

  // Use default message but allow editing
  const defaultMessage = $derived(`Merge branch '${headBranch}' into ${baseBranch}`);
  let commitMessage = $state('');
  let messageInitialized = false;

  $effect(() => {
    if (!messageInitialized && defaultMessage) {
      commitMessage = defaultMessage;
      messageInitialized = true;
    }
  });

  let mergeInfo = $state<{
    canMerge: boolean;
    conflicts: string[];
    isFastForward: boolean;
    stats: { additions: number; deletions: number; files: string[] };
  } | null>(null);

  // Load merge info
  $effect(() => {
    if (!dirCid || !baseBranch || !headBranch) return;

    // Don't reload merge info if merge was already successful
    // (dirCid changes after merge due to autosave)
    if (mergeSuccess) return;

    loading = true;
    error = null;
    mergeInfo = null;
    mergeError = null;

    let cancelled = false;

    (async () => {
      try {
        // Get diff stats
        const diffResult = await diffBranches(dirCid, baseBranch, headBranch);
        if (cancelled) return;

        if (diffResult.error) {
          error = diffResult.error;
          loading = false;
          return;
        }

        // Check if merge is possible
        const mergeCheck = await canMerge(dirCid, baseBranch, headBranch);
        if (cancelled) return;

        if (mergeCheck.error) {
          error = mergeCheck.error;
          loading = false;
          return;
        }

        mergeInfo = {
          canMerge: mergeCheck.canMerge,
          conflicts: mergeCheck.conflicts,
          isFastForward: mergeCheck.isFastForward,
          stats: diffResult.stats,
        };
        loading = false;
      } catch (err) {
        if (!cancelled) {
          error = getErrorMessage(err);
          loading = false;
        }
      }
    })();

    return () => { cancelled = true; };
  });

  // Perform the merge
  async function handleMerge() {
    if (!dirCid || !mergeInfo?.canMerge || merging) return;

    merging = true;
    mergeError = null;

    try {
      const authorName = nostr?.npub?.slice(0, 12) || 'User';
      const authorEmail = 'user@hashtree.org';

      const result = await mergeBranches(
        dirCid,
        baseBranch,
        headBranch,
        commitMessage,
        authorName,
        authorEmail
      );

      if (!result.success) {
        mergeError = result.error || 'Merge failed';
        merging = false;
        return;
      }

      // Apply the git changes and working files to get new directory CID
      if (result.gitFiles) {
        const { getCurrentRootCid } = await import('../../actions/route');
        const { getTree } = await import('../../store');
        const { LinkType } = await import('hashtree');

        const tree = getTree();
        let newDirCid = await applyGitChanges(dirCid, result.gitFiles);

        // Also apply working directory files that were changed by the merge
        if (result.workingFiles) {
          for (const file of result.workingFiles) {
            if (file.isDir) continue; // Skip directories, they'll be created with files

            const pathParts = file.name.split('/');
            const fileName = pathParts.pop()!;
            const filePath = pathParts;

            const { cid: fileCid, size } = await tree.putFile(file.data);
            newDirCid = await tree.setEntry(newDirCid, filePath, fileName, fileCid, size, LinkType.Blob);
          }
        }

        // Get current tree root and update it with the new directory
        const treeRootCid = getCurrentRootCid();
        if (treeRootCid) {
          let newRootCid;
          const currentPath = route.path;

          if (currentPath.length === 0) {
            // Git repo is at tree root
            newRootCid = newDirCid;
          } else {
            // Git repo is in a subdirectory - replace it at that path
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

          // Save and publish
          autosaveIfOwn(newRootCid);
        }
      }

      // Update PR status if this merge is from a PR
      if (prEventId && prAuthorPubkey) {
        try {
          await updateStatus(prEventId, prAuthorPubkey, 'merged');
        } catch (err) {
          console.error('Failed to update PR status:', err);
          // Don't fail the merge if status update fails
        }
      }

      mergeSuccess = true;
      merging = false;
    } catch (err) {
      mergeError = getErrorMessage(err);
      merging = false;
    }
  }
</script>

<!-- File browser on left - hidden on mobile -->
<div class="hidden lg:flex lg:w-80 shrink-0 lg:border-r border-surface-3 flex-col min-h-0">
  <FileBrowser />
</div>

<!-- Right panel with merge UI - shown on mobile -->
<div class="flex flex-1 flex-col min-w-0 min-h-0 bg-surface-0">
  <!-- Header with back button -->
  <ViewerHeader
    {backUrl}
    {npub}
    {rootCid}
    visibility={currentTree?.visibility}
    icon="i-lucide-git-merge text-success"
    name="Merge"
  />

  <!-- Tab navigation -->
  <RepoTabNav {npub} {repoName} activeTab="code" />

  <!-- Content -->
  <div class="flex-1 overflow-auto p-4">
    {#if loading}
      <div class="flex items-center justify-center py-12 text-text-3">
        <span class="i-lucide-loader-2 animate-spin mr-2"></span>
        Checking merge status...
      </div>
    {:else if error}
      <div class="flex flex-col items-center justify-center py-12 text-danger">
        <span class="i-lucide-alert-circle text-2xl mb-2"></span>
        <span>{error}</span>
      </div>
    {:else if mergeSuccess}
      <div class="flex flex-col items-center justify-center py-12">
        <span class="i-lucide-check-circle text-4xl mb-4 text-success"></span>
        <h2 class="text-xl font-semibold mb-2 text-success">Merge successful!</h2>
        <p class="text-text-2 mb-4">
          Branch <span class="font-mono text-accent">{headBranch}</span> has been merged into <span class="font-mono">{baseBranch}</span>
        </p>
        <a href={backUrl} class="btn-primary">
          <span class="i-lucide-arrow-left mr-2"></span>
          Back to repository
        </a>
      </div>
    {:else if mergeInfo}
      <!-- Merge preview -->
      <div class="bg-surface-1 rounded-lg b-1 b-solid b-surface-3 overflow-hidden mb-4">
        <div class="p-4">
          <!-- Branch names -->
          <h2 class="text-lg font-semibold mb-4">
            Merge <span class="text-accent font-mono">{headBranch}</span> into <span class="font-mono">{baseBranch}</span>
          </h2>

          <!-- Merge type indicator -->
          {#if mergeInfo.isFastForward}
            <div class="text-sm text-success flex items-center gap-2 mb-4 bg-surface-2 p-3 rounded">
              <span class="i-lucide-fast-forward"></span>
              <div>
                <span class="font-medium">Fast-forward merge</span>
                <p class="text-text-3">The base branch can be directly updated to include all commits from the head branch.</p>
              </div>
            </div>
          {:else}
            <div class="text-sm text-text-2 flex items-center gap-2 mb-4 bg-surface-2 p-3 rounded">
              <span class="i-lucide-git-merge"></span>
              <div>
                <span class="font-medium">Merge commit</span>
                <p class="text-text-3">A new merge commit will be created to combine both branches.</p>
              </div>
            </div>
          {/if}

          <!-- Conflict warning -->
          {#if !mergeInfo.canMerge && mergeInfo.conflicts.length > 0}
            <div class="text-sm text-danger flex items-start gap-2 mb-4 bg-red-900/20 p-3 rounded b-1 b-solid b-danger">
              <span class="i-lucide-alert-triangle mt-0.5"></span>
              <div>
                <span class="font-medium">Merge conflicts detected</span>
                <p class="text-text-2 mt-1">The following files have conflicts that must be resolved:</p>
                <ul class="mt-2 font-mono text-xs">
                  {#each mergeInfo.conflicts as conflict (conflict)}
                    <li class="text-danger">{conflict}</li>
                  {/each}
                </ul>
              </div>
            </div>
          {/if}

          <!-- Stats summary -->
          <div class="flex items-center gap-4 text-sm text-text-2 mb-4">
            <span>
              <span class="font-medium">{mergeInfo.stats.files.length}</span> file{mergeInfo.stats.files.length !== 1 ? 's' : ''} will be changed
            </span>
            {#if mergeInfo.stats.additions > 0}
              <span class="text-success">+{mergeInfo.stats.additions}</span>
            {/if}
            {#if mergeInfo.stats.deletions > 0}
              <span class="text-error">-{mergeInfo.stats.deletions}</span>
            {/if}
          </div>

          <!-- Commit message input (only for non-fast-forward) -->
          {#if !mergeInfo.isFastForward && mergeInfo.canMerge}
            <div class="mb-4">
              <label for="commit-message" class="block text-sm font-medium text-text-2 mb-2">
                Merge commit message
              </label>
              <textarea
                id="commit-message"
                bind:value={commitMessage}
                rows="3"
                class="w-full px-3 py-2 text-sm bg-surface-0 b-1 b-solid b-surface-3 rounded focus:outline-none focus:b-accent font-mono"
              ></textarea>
            </div>
          {/if}

          <!-- Merge error -->
          {#if mergeError}
            <div class="text-sm text-danger flex items-center gap-2 mb-4 bg-red-900/20 p-3 rounded">
              <span class="i-lucide-alert-circle"></span>
              {mergeError}
            </div>
          {/if}

          <!-- Action buttons -->
          <div class="flex items-center gap-3">
            {#if mergeInfo.canMerge && canMergeCheck}
              <button
                onclick={handleMerge}
                disabled={merging}
                class="btn-success flex items-center gap-2"
              >
                {#if merging}
                  <span class="i-lucide-loader-2 animate-spin"></span>
                  Merging...
                {:else}
                  <span class="i-lucide-git-merge"></span>
                  Confirm merge
                {/if}
              </button>
            {:else if !canMergeCheck}
              <div class="text-sm text-text-3">
                You don't have permission to merge into this repository.
              </div>
            {:else}
              <button disabled class="btn-ghost opacity-50 cursor-not-allowed flex items-center gap-2">
                <span class="i-lucide-git-merge"></span>
                Cannot merge (conflicts)
              </button>
            {/if}
            <a href={backUrl} class="btn-ghost">
              Cancel
            </a>
          </div>
        </div>
      </div>

      <!-- Files that will be changed -->
      {#if mergeInfo.stats.files.length > 0}
        <div class="bg-surface-1 rounded-lg b-1 b-solid b-surface-3 overflow-hidden">
          <div class="px-4 py-2 b-b-1 b-b-solid b-b-surface-3 flex items-center gap-2">
            <span class="i-lucide-files text-text-3"></span>
            <span class="text-sm font-medium">Files to be changed</span>
          </div>
          <div class="p-2 max-h-64 overflow-auto">
            {#each mergeInfo.stats.files as file (file)}
              <div class="px-2 py-1 text-sm font-mono text-text-2 hover:bg-surface-2 rounded">
                {file}
              </div>
            {/each}
          </div>
        </div>
      {/if}
    {/if}
  </div>
</div>
