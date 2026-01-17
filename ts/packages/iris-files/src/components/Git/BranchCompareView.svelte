<script lang="ts">
  /**
   * BranchCompareView - Shows diff between two branches for PR review
   * Displays branch names, diff statistics, and colorized diff output
   */
  import { diffBranches } from '../../utils/git';
  import { getErrorMessage } from '../../utils/errorMessage';
  import { routeStore, treeRootStore, createTreesStore, currentDirCidStore } from '../../stores';
  import { nostrStore } from '../../nostr';
  import { navigate } from '../../lib/router.svelte';
  import FileBrowser from '../FileBrowser.svelte';
  import ViewerHeader from '../Viewer/ViewerHeader.svelte';
  import RepoTabNav from './RepoTabNav.svelte';

  interface Props {
    npub: string;
    repoName: string;
    baseBranch: string;
    headBranch: string;
  }

  let { npub, repoName, baseBranch, headBranch }: Props = $props();

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

  // Build back URL (to code tab) - use repoName which includes full path to git repo
  let backUrl = $derived.by(() => {
    const linkKeySuffix = route.params.get('k') ? `?k=${route.params.get('k')}` : '';
    return `#/${npub}/${repoName}${linkKeySuffix}`;
  });

  // Check if current user can merge (owns the repo)
  let nostr = $derived($nostrStore);
  let canMerge = $derived(nostr?.npub === npub);

  // Diff data state
  let loading = $state(true);
  let error = $state<string | null>(null);
  let diffData = $state<{
    diff: string;
    stats: { additions: number; deletions: number; files: string[] };
    canFastForward: boolean;
  } | null>(null);

  // Track if we're waiting for dirCid
  let waitingForDir = $state(true);

  // Load diff data
  $effect(() => {
    // If we have branch params but no dirCid, we're still waiting
    if (!dirCid) {
      waitingForDir = true;
      return;
    }
    waitingForDir = false;

    if (!baseBranch || !headBranch) return;

    loading = true;
    error = null;
    diffData = null;

    let cancelled = false;

    (async () => {
      try {
        const result = await diffBranches(dirCid, baseBranch, headBranch);

        if (cancelled) return;

        if (result.error) {
          error = result.error;
          loading = false;
          return;
        }

        diffData = {
          diff: result.diff,
          stats: result.stats,
          canFastForward: result.canFastForward,
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

  // Colorize diff output
  function colorizeDiff(diff: string): string {
    return diff.split('\n').map(line => {
      const escaped = line
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;');

      if (line.startsWith('+') && !line.startsWith('+++')) {
        return `<span class="text-success">${escaped}</span>`;
      }
      if (line.startsWith('-') && !line.startsWith('---')) {
        return `<span class="text-error">${escaped}</span>`;
      }
      if (line.startsWith('@@')) {
        return `<span class="text-accent">${escaped}</span>`;
      }
      if (line.startsWith('diff --git') || line.startsWith('index ') || line.startsWith('---') || line.startsWith('+++')) {
        return `<span class="text-text-3">${escaped}</span>`;
      }
      return escaped;
    }).join('\n');
  }

  // Navigate to merge view
  function goToMerge() {
    const linkKeySuffix = route.params.get('k') ? `&k=${route.params.get('k')}` : '';
    navigate(`/${npub}/${repoName}?merge=1&base=${baseBranch}&head=${headBranch}${linkKeySuffix}`);
  }
</script>

<!-- File browser on left - hidden on mobile -->
<div class="hidden lg:flex lg:w-80 shrink-0 lg:border-r border-surface-3 flex-col min-h-0">
  <FileBrowser />
</div>

<!-- Right panel with diff details - shown on mobile -->
<div class="flex flex-1 flex-col min-w-0 min-h-0 bg-surface-0">
  <!-- Header with back button -->
  <ViewerHeader
    {backUrl}
    {npub}
    {rootCid}
    visibility={currentTree?.visibility}
    icon="i-lucide-git-compare text-accent"
    name="Compare"
  />

  <!-- Tab navigation -->
  <RepoTabNav {npub} {repoName} activeTab="code" />

  <!-- Content -->
  <div class="flex-1 overflow-auto p-4">
    {#if waitingForDir}
      <div class="flex items-center justify-center py-12 text-text-3">
        <span class="i-lucide-loader-2 animate-spin mr-2"></span>
        Loading repository...
      </div>
    {:else if loading}
      <div class="flex items-center justify-center py-12 text-text-3">
        <span class="i-lucide-loader-2 animate-spin mr-2"></span>
        Comparing branches...
      </div>
    {:else if error}
      <div class="flex flex-col items-center justify-center py-12 text-danger">
        <span class="i-lucide-alert-circle text-2xl mb-2"></span>
        <span>{error}</span>
      </div>
    {:else if diffData}
      <!-- Branch comparison header -->
      <div class="bg-surface-1 rounded-lg b-1 b-solid b-surface-3 overflow-hidden mb-4">
        <div class="p-4">
          <!-- Branch names -->
          <div class="flex items-center gap-3 mb-4">
            <div class="flex items-center gap-2 bg-surface-2 px-3 py-1.5 rounded">
              <span class="i-lucide-git-branch text-text-3"></span>
              <span class="font-mono text-sm">{baseBranch}</span>
            </div>
            <span class="i-lucide-arrow-left text-text-3"></span>
            <div class="flex items-center gap-2 bg-surface-2 px-3 py-1.5 rounded">
              <span class="i-lucide-git-branch text-accent"></span>
              <span class="font-mono text-sm text-accent">{headBranch}</span>
            </div>
          </div>

          <!-- Fast-forward indicator -->
          {#if diffData.canFastForward}
            <div class="text-sm text-success flex items-center gap-2 mb-4">
              <span class="i-lucide-check-circle"></span>
              This branch can be fast-forwarded
            </div>
          {/if}

          <!-- Actions -->
          {#if canMerge}
            <button
              onclick={goToMerge}
              class="btn-primary flex items-center gap-2"
            >
              <span class="i-lucide-git-merge"></span>
              Merge into {baseBranch}
            </button>
          {/if}
        </div>

        <!-- Stats bar -->
        <div class="px-4 py-2 bg-surface-2 flex items-center gap-4 text-sm">
          <span class="text-text-2">
            <span class="font-medium">{diffData.stats.files.length}</span> file{diffData.stats.files.length !== 1 ? 's' : ''} changed
          </span>
          {#if diffData.stats.additions > 0}
            <span class="text-success">
              +{diffData.stats.additions}
            </span>
          {/if}
          {#if diffData.stats.deletions > 0}
            <span class="text-error">
              -{diffData.stats.deletions}
            </span>
          {/if}
        </div>
      </div>

      <!-- Changed files list -->
      {#if diffData.stats.files.length > 0}
        <div class="bg-surface-1 rounded-lg b-1 b-solid b-surface-3 overflow-hidden mb-4">
          <div class="px-4 py-2 b-b-1 b-b-solid b-b-surface-3 flex items-center gap-2">
            <span class="i-lucide-files text-text-3"></span>
            <span class="text-sm font-medium">Changed files</span>
          </div>
          <div class="p-2">
            {#each diffData.stats.files as file (file)}
              <div class="px-2 py-1 text-sm font-mono text-text-2 hover:bg-surface-2 rounded">
                {file}
              </div>
            {/each}
          </div>
        </div>
      {/if}

      <!-- Diff -->
      {#if diffData.diff}
        <div class="bg-surface-1 rounded-lg b-1 b-solid b-surface-3 overflow-hidden">
          <div class="px-4 py-2 b-b-1 b-b-solid b-b-surface-3 flex items-center gap-2">
            <span class="i-lucide-file-diff text-text-3"></span>
            <span class="text-sm font-medium">Diff</span>
          </div>
          <!-- eslint-disable-next-line svelte/no-at-html-tags -- colorizeDiff escapes HTML -->
          <pre class="p-4 text-xs font-mono overflow-x-auto whitespace-pre">{@html colorizeDiff(diffData.diff)}</pre>
        </div>
      {:else}
        <div class="bg-surface-1 rounded-lg b-1 b-solid b-surface-3 p-8 text-center text-text-3">
          <span class="i-lucide-check text-2xl mb-2"></span>
          <p>No differences between branches</p>
        </div>
      {/if}
    {/if}
  </div>
</div>
