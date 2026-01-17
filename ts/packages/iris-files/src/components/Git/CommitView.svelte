<script lang="ts">
  /**
   * CommitView - Shows details of a single git commit
   * Displays commit message, author, date, and diff
   * Uses getLog to find commits (since git show is not supported by wasm-git)
   */
  import { SvelteSet } from 'svelte/reactivity';
  import { getLog, runGitCommand } from '../../utils/git';
  import { getErrorMessage } from '../../utils/errorMessage';
  import { routeStore, treeRootStore, createTreesStore, currentDirCidStore } from '../../stores';
  import FileBrowser from '../FileBrowser.svelte';
  import ViewerHeader from '../Viewer/ViewerHeader.svelte';
  import RepoTabNav from './RepoTabNav.svelte';

  interface Props {
    npub: string;
    repoName: string;
    commitHash: string;
  }

  let { npub, repoName, commitHash }: Props = $props();

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

  // Commit data state
  let loading = $state(true);
  let error = $state<string | null>(null);
  let commitData = $state<{
    hash: string;
    author: string;
    email: string;
    date: string;
    message: string;
    diff: string;
    stats: { additions: number; deletions: number; files: number };
  } | null>(null);

  // Load commit data
  $effect(() => {
    if (!dirCid || !commitHash) return;

    loading = true;
    error = null;
    commitData = null;

    let cancelled = false;

    (async () => {
      try {
        // Get all commits using getLog (git show not supported by wasm-git)
        const commits = await getLog(dirCid, { depth: 1000 });

        if (cancelled) return;

        // Support HEAD as commitHash
        const targetHash = commitHash === 'HEAD' && commits.length > 0
          ? commits[0].oid
          : commitHash;

        // Find the commit by hash (support short or full hash)
        const commit = commits.find(c =>
          c.oid === targetHash || c.oid.startsWith(targetHash)
        );

        if (!commit) {
          error = `Commit ${commitHash} not found`;
          loading = false;
          return;
        }

        // Format date from timestamp
        const date = new Date(commit.timestamp * 1000).toISOString();

        // Try to get diff using git diff command
        let diff = '';
        let stats = { additions: 0, deletions: 0, files: 0 };

        // Find parent commit index
        const commitIndex = commits.findIndex(c => c.oid === commit.oid);
        const parentCommit = commitIndex >= 0 && commitIndex < commits.length - 1
          ? commits[commitIndex + 1]
          : null;

        try {
          // Try git diff between parent and this commit
          if (parentCommit) {
            const diffResult = await runGitCommand(dirCid, `diff ${parentCommit.oid} ${commit.oid}`);
            if (!diffResult.error && diffResult.output) {
              diff = diffResult.output;
              // Parse stats from diff
              const diffLines = diff.split('\n');
              const filesSet = new SvelteSet<string>();
              for (const line of diffLines) {
                if (line.startsWith('diff --git')) {
                  const match = line.match(/diff --git a\/(.*) b\/(.*)/);
                  if (match) filesSet.add(match[2]);
                } else if (line.startsWith('+') && !line.startsWith('+++')) {
                  stats.additions++;
                } else if (line.startsWith('-') && !line.startsWith('---')) {
                  stats.deletions++;
                }
              }
              stats.files = filesSet.size;
            }
          } else {
            // First commit - show all files as added (use diff-tree for initial commit)
            const diffResult = await runGitCommand(dirCid, `diff-tree --root -p ${commit.oid}`);
            if (!diffResult.error && diffResult.output) {
              diff = diffResult.output;
              const diffLines = diff.split('\n');
              const filesSet = new SvelteSet<string>();
              for (const line of diffLines) {
                if (line.startsWith('diff --git')) {
                  const match = line.match(/diff --git a\/(.*) b\/(.*)/);
                  if (match) filesSet.add(match[2]);
                } else if (line.startsWith('+') && !line.startsWith('+++')) {
                  stats.additions++;
                }
              }
              stats.files = filesSet.size;
            }
          }
        } catch {
          // Diff not available - that's okay
        }

        if (cancelled) return;

        commitData = {
          hash: commit.oid,
          author: commit.author,
          email: commit.email,
          date,
          message: commit.message,
          diff,
          stats,
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

  // Format date
  function formatDate(isoDate: string): string {
    try {
      const date = new Date(isoDate);
      return date.toLocaleDateString('en-US', {
        year: 'numeric',
        month: 'short',
        day: 'numeric',
        hour: '2-digit',
        minute: '2-digit',
      });
    } catch {
      return isoDate;
    }
  }

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

  // Build browse files URL (links to code tab)
  let browseFilesUrl = $derived(backUrl);
</script>

<!-- File browser on left - hidden on mobile -->
<div class="hidden lg:flex lg:w-80 shrink-0 lg:border-r border-surface-3 flex-col min-h-0">
  <FileBrowser />
</div>

<!-- Right panel with commit details - shown on mobile -->
<div class="flex flex-1 flex-col min-w-0 min-h-0 bg-surface-0">
  <!-- Header with back button -->
  <ViewerHeader
    {backUrl}
    {npub}
    {rootCid}
    visibility={currentTree?.visibility}
    icon="i-lucide-git-commit text-warning"
    name={commitHash.slice(0, 7)}
  />

  <!-- Tab navigation -->
  <RepoTabNav {npub} {repoName} activeTab="code" />

  <!-- Content -->
  <div class="flex-1 overflow-auto p-4">
    {#if loading}
      <div class="flex items-center justify-center py-12 text-text-3">
        <span class="i-lucide-loader-2 animate-spin mr-2"></span>
        Loading commit...
      </div>
    {:else if error}
      <div class="flex flex-col items-center justify-center py-12 text-danger">
        <span class="i-lucide-alert-circle text-2xl mb-2"></span>
        <span>{error}</span>
      </div>
    {:else if commitData}
      <!-- Commit header -->
      <div class="bg-surface-1 rounded-lg b-1 b-solid b-surface-3 overflow-hidden mb-4">
        <div class="p-4">
          <!-- Commit message -->
          <h1 class="text-lg font-semibold text-text-1 mb-3 whitespace-pre-wrap">{commitData.message.split('\n')[0]}</h1>

          {#if commitData.message.includes('\n')}
            <pre class="text-sm text-text-2 whitespace-pre-wrap mb-4">{commitData.message.split('\n').slice(1).join('\n').trim()}</pre>
          {/if}

          <!-- Author and date -->
          <div class="flex flex-wrap items-center gap-4 text-sm text-text-2">
            <div class="flex items-center gap-2">
              <span class="i-lucide-user text-text-3"></span>
              <span class="font-medium">{commitData.author}</span>
              {#if commitData.email}
                <span class="text-text-3">&lt;{commitData.email}&gt;</span>
              {/if}
            </div>
            <div class="flex items-center gap-2">
              <span class="i-lucide-calendar text-text-3"></span>
              <span>{formatDate(commitData.date)}</span>
            </div>
          </div>

          <!-- Commit hash and actions -->
          <div class="flex flex-wrap items-center gap-3 mt-4 pt-4 b-t-1 b-t-solid b-t-surface-3">
            <code class="text-xs bg-surface-2 px-2 py-1 rounded font-mono">{commitData.hash}</code>
            <a href={browseFilesUrl} class="btn-ghost text-sm flex items-center gap-1">
              <span class="i-lucide-folder"></span>
              Browse files
            </a>
          </div>
        </div>

        <!-- Stats bar -->
        <div class="px-4 py-2 bg-surface-2 flex items-center gap-4 text-sm">
          <span class="text-text-2">
            <span class="font-medium">{commitData.stats.files}</span> file{commitData.stats.files !== 1 ? 's' : ''} changed
          </span>
          {#if commitData.stats.additions > 0}
            <span class="text-success">
              +{commitData.stats.additions}
            </span>
          {/if}
          {#if commitData.stats.deletions > 0}
            <span class="text-error">
              -{commitData.stats.deletions}
            </span>
          {/if}
        </div>
      </div>

      <!-- Diff -->
      {#if commitData.diff}
        <div class="bg-surface-1 rounded-lg b-1 b-solid b-surface-3 overflow-hidden">
          <div class="px-4 py-2 b-b-1 b-b-solid b-b-surface-3 flex items-center gap-2">
            <span class="i-lucide-file-diff text-text-3"></span>
            <span class="text-sm font-medium">Diff</span>
          </div>
          <!-- eslint-disable-next-line svelte/no-at-html-tags -- colorizeDiff escapes HTML -->
          <pre class="p-4 text-xs font-mono overflow-x-auto whitespace-pre">{@html colorizeDiff(commitData.diff)}</pre>
        </div>
      {/if}
    {/if}
  </div>
</div>
