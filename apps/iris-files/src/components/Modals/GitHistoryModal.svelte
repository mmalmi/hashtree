<script lang="ts" module>
  /**
   * Modal for displaying git commit history
   */
  import type { CID } from 'hashtree';

  export interface GitHistoryTarget {
    dirCid: CID;
    canEdit: boolean;
    onCheckout?: (commitSha: string) => Promise<void>;
  }

  let show = $state(false);
  let target = $state<GitHistoryTarget | null>(null);

  export function open(t: GitHistoryTarget) {
    target = t;
    show = true;
  }

  export function close() {
    show = false;
    target = null;
  }
</script>

<script lang="ts">
  import { createGitLogStore, type CommitInfo } from '../../stores/git';
  import { createCIStatusStore, loadCIConfig, type CIStatus, type CIConfig } from '../../stores/ci';
  import { routeStore } from '../../stores';
  import { nhashEncode } from 'hashtree';
  import { checkoutCommit, getBranches } from '../../utils/git';
  import { getErrorMessage } from '../../utils/errorMessage';
  import CIStatusBadge from '../Git/CIStatusBadge.svelte';
  import { open as openCIRunsModal } from './CIRunsModal.svelte';
  import InfiniteScroll from '../InfiniteScroll.svelte';

  // Get route info for building commit view URLs
  let route = $derived($routeStore);
  let repoName = $derived(route.treeName ? route.treeName + (route.path.length > 0 ? '/' + route.path.join('/') : '') : null);
  let gitRootPath = $derived(route.params.get('g'));
  let repoPath = $derived.by(() => {
    if (!route.treeName) return '';
    const gitPath = gitRootPath !== null
      ? (gitRootPath === '' ? [] : gitRootPath.split('/'))
      : route.path;
    if (gitPath.length === 0) return route.treeName;
    return `${route.treeName}/${gitPath.join('/')}`;
  });

  function bytesToHex(bytes: Uint8Array): string {
    return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
  }

  // Commit loading state
  let currentDepth = $state(50);
  let logStore = $derived(target ? createGitLogStore(target.dirCid, currentDepth) : null);
  let logState = $state<{ commits: CommitInfo[]; headOid: string | null; loading: boolean; error: string | null }>({
    commits: [],
    headOid: null,
    loading: true,
    error: null,
  });
  let loadingMore = $state(false);
  // Deduplicate commits by OID (can happen with merge commits in history)
  let uniqueCommits = $derived(() => {
    const seen = new Set<string>();
    return logState.commits.filter(c => {
      if (seen.has(c.oid)) return false;
      seen.add(c.oid);
      return true;
    });
  });
  let hasMoreCommits = $derived(logState.commits.length >= currentDepth && !logState.loading);

  // Branch info for detached HEAD detection
  let branchInfo = $state<{ branches: string[]; currentBranch: string | null }>({ branches: [], currentBranch: null });
  let isDetachedHead = $derived(branchInfo.currentBranch === null && logState.commits.length > 0);

  let checkoutInProgress = $state<string | null>(null);
  let checkoutError = $state<string | null>(null);

  let ciConfig = $state<CIConfig | null>(null);
  let ciStatusByCommit = $state<Map<string, CIStatus>>(new Map());
  // eslint-disable-next-line svelte/prefer-svelte-reactivity -- avoid reactive Map feedback loops in subscriptions
  let ciStatusMap = new Map<string, CIStatus>();
  // eslint-disable-next-line svelte/prefer-svelte-reactivity -- avoid reactive Map feedback loops in subscriptions
  let ciStatusUnsubs = new Map<string, () => void>();
  const CI_CONFIG_MAX_ATTEMPTS = 3;
  const CI_CONFIG_RETRY_MS = 1500;
  let ciConfigRetryAttempts = 0;
  let ciConfigRetryTimer: ReturnType<typeof setTimeout> | null = null;

  $effect(() => {
    if (!logStore) {
      logState = { commits: [], headOid: null, loading: false, error: null };
      return;
    }
    const unsub = logStore.subscribe(value => {
      logState = value;
      loadingMore = false;
    });
    return unsub;
  });

  // Reset depth when modal closes
  $effect(() => {
    if (!show) {
      currentDepth = 50;
    }
  });

  function loadMoreCommits() {
    loadingMore = true;
    currentDepth += 100;
  }

  // Fetch branch info when modal opens
  $effect(() => {
    if (!target) {
      branchInfo = { branches: [], currentBranch: null };
      return;
    }
    getBranches(target.dirCid).then(info => {
      branchInfo = info;
    });
  });

  $effect(() => {
    ciConfig = null;
    ciConfigRetryAttempts = 0;
    if (ciConfigRetryTimer) {
      clearTimeout(ciConfigRetryTimer);
      ciConfigRetryTimer = null;
    }
    if (!target) return;
    let cancelled = false;
    const loadConfig = async () => {
      ciConfigRetryAttempts += 1;
      try {
        const result = await loadCIConfig(target.dirCid);
        if (cancelled) return;
        if (!result && ciConfigRetryAttempts < CI_CONFIG_MAX_ATTEMPTS) {
          ciConfigRetryTimer = setTimeout(loadConfig, CI_CONFIG_RETRY_MS);
          return;
        }
        ciConfig = result;
      } catch {
        if (cancelled) return;
        if (ciConfigRetryAttempts < CI_CONFIG_MAX_ATTEMPTS) {
          ciConfigRetryTimer = setTimeout(loadConfig, CI_CONFIG_RETRY_MS);
          return;
        }
        ciConfig = null;
      }
    };
    loadConfig();
    return () => {
      cancelled = true;
      if (ciConfigRetryTimer) {
        clearTimeout(ciConfigRetryTimer);
        ciConfigRetryTimer = null;
      }
    };
  });

  $effect(() => {
    ciStatusMap = new Map();
    ciStatusByCommit = new Map();
    ciStatusUnsubs.forEach(unsub => unsub());
    ciStatusUnsubs.clear();

    if (!show || !ciConfig || !repoPath || !ciConfig.runners || ciConfig.runners.length === 0) {
      return;
    }

    if (logState.commits.length === 0) return;

    for (const commit of logState.commits) {
      const store = createCIStatusStore(repoPath, commit.oid, ciConfig.runners);
      const unsub = store.subscribe(value => {
        ciStatusMap.set(commit.oid, value);
        ciStatusByCommit = new Map(ciStatusMap);
      });
      ciStatusUnsubs.set(commit.oid, unsub);
    }

    return () => {
      ciStatusUnsubs.forEach(unsub => unsub());
      ciStatusUnsubs.clear();
    };
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

  // Format timestamp
  function formatDate(timestamp: number): string {
    const date = new Date(timestamp * 1000);
    const now = new Date();
    const diff = now.getTime() - date.getTime();
    const days = Math.floor(diff / (1000 * 60 * 60 * 24));

    if (days === 0) {
      const hours = Math.floor(diff / (1000 * 60 * 60));
      if (hours === 0) {
        const mins = Math.floor(diff / (1000 * 60));
        return mins <= 1 ? 'just now' : `${mins} minutes ago`;
      }
      return hours === 1 ? '1 hour ago' : `${hours} hours ago`;
    }
    if (days === 1) return 'yesterday';
    if (days < 7) return `${days} days ago`;
    if (days < 30) return `${Math.floor(days / 7)} weeks ago`;
    if (days < 365) return `${Math.floor(days / 30)} months ago`;
    return `${Math.floor(days / 365)} years ago`;
  }

  // Get first line of commit message
  function getCommitTitle(message: string): string {
    return message.split('\n')[0].slice(0, 72);
  }

  // Get short commit hash
  function shortHash(oid: string): string {
    return oid.slice(0, 7);
  }

  function handleCIClick(commitOid: string, event: MouseEvent) {
    event.stopPropagation();
    const status = ciStatusByCommit.get(commitOid);
    if (!status || status.loading || status.jobs.length === 0) return;
    openCIRunsModal({ status, repoPath });
  }

  // Handle checkout button click (for own repos)
  async function handleCheckout(commitSha: string) {
    if (!target || checkoutInProgress) return;

    checkoutInProgress = commitSha;
    checkoutError = null;

    try {
      if (target.onCheckout) {
        await target.onCheckout(commitSha);
        close();
      }
    } catch (err) {
      checkoutError = getErrorMessage(err);
    } finally {
      checkoutInProgress = null;
    }
  }

  // Handle checkout branch (for returning from detached HEAD)
  async function handleCheckoutBranch(branchName: string) {
    if (!target || checkoutInProgress) return;

    checkoutInProgress = branchName;
    checkoutError = null;

    try {
      if (target.onCheckout) {
        // Checkout branch by name (git will resolve to the branch's HEAD)
        await target.onCheckout(branchName);
        close();
      }
    } catch (err) {
      checkoutError = getErrorMessage(err);
    } finally {
      checkoutInProgress = null;
    }
  }

  // Handle browse button click (for others' repos)
  async function handleBrowse(commitSha: string) {
    if (!target) return;

    try {
      // Checkout to get the CID at that commit
      const newCid = await checkoutCommit(target.dirCid, commitSha);

      // Convert CID to nhash
      const hashHex = bytesToHex(newCid.hash);
      const keyHex = newCid.key ? bytesToHex(newCid.key) : undefined;
      const nhash = nhashEncode({ hash: hashHex, decryptKey: keyHex });

      // Open in new tab
      window.open(`#/${nhash}`, '_blank');
    } catch (err) {
      checkoutError = getErrorMessage(err);
    }
  }
</script>

{#if show && target}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div class="fixed inset-0 z-50 flex items-center justify-center bg-black/50" onclick={close} data-testid="git-history-modal">
    <div class="bg-surface-1 rounded-lg shadow-lg w-full max-w-2xl mx-4 max-h-[80vh] flex flex-col" onclick={(e) => e.stopPropagation()}>
      <!-- Header -->
      <div class="flex items-center justify-between p-4 b-b-1 b-b-solid b-b-surface-3">
        <h2 class="text-lg font-semibold flex items-center gap-2">
          <span class="i-lucide-history"></span>
          Commit History
        </h2>
        <button onclick={close} class="btn-ghost p-1" aria-label="Close">
          <span class="i-lucide-x text-lg"></span>
        </button>
      </div>

      <!-- Content -->
      <div class="flex-1 overflow-auto p-4">
        {#if logState.loading}
          <div class="flex items-center justify-center py-8 text-text-3">
            <span class="i-lucide-loader-2 animate-spin mr-2"></span>
            Loading commits...
          </div>
        {:else if logState.error}
          <div class="flex items-center justify-center py-8 text-error">
            <span class="i-lucide-alert-circle mr-2"></span>
            {logState.error}
          </div>
        {:else if logState.commits.length === 0}
          <div class="flex items-center justify-center py-8 text-text-3">
            No commits found
          </div>
        {:else}
          {#if checkoutError}
            <div class="mb-4 p-3 bg-error/10 text-error rounded-lg text-sm flex items-center gap-2">
              <span class="i-lucide-alert-circle"></span>
              {checkoutError}
            </div>
          {/if}
          {#if isDetachedHead && target?.canEdit && branchInfo.branches.length > 0}
            <div class="mb-4 p-3 bg-warning/10 text-warning rounded-lg text-sm">
              <div class="flex items-center gap-2 mb-2">
                <span class="i-lucide-alert-triangle"></span>
                <span class="font-medium">Detached HEAD</span>
              </div>
              <p class="text-text-2 text-xs mb-2">
                You're viewing an older commit. Switch to a branch to see all commits:
              </p>
              <div class="flex flex-wrap gap-2">
                {#each branchInfo.branches as branch (branch)}
                  <button
                    onclick={() => handleCheckoutBranch(branch)}
                    disabled={checkoutInProgress !== null}
                    class="btn-ghost px-2 py-1 text-xs flex items-center gap-1 bg-surface-2"
                  >
                    {#if checkoutInProgress === branch}
                      <span class="i-lucide-loader-2 animate-spin"></span>
                    {:else}
                      <span class="i-lucide-git-branch"></span>
                    {/if}
                    {branch}
                  </button>
                {/each}
              </div>
            </div>
          {/if}
          <InfiniteScroll onLoadMore={loadMoreCommits} loading={loadingMore || !hasMoreCommits}>
              <div class="flex flex-col">
                {#each uniqueCommits() as commit, i (commit.oid)}
                  {@const isHead = commit.oid === logState.headOid}
                  {@const ciStatus = ciStatusByCommit.get(commit.oid) ?? null}
                  {@const commitsArray = uniqueCommits()}
                  <div class="flex gap-3 pb-4 {i < commitsArray.length - 1 || hasMoreCommits ? 'b-b-1 b-b-solid b-b-surface-3 mb-4' : ''} {isHead ? 'bg-accent/5 -mx-4 px-4 py-3 rounded-lg' : ''}">
                    <!-- Timeline dot -->
                    <div class="flex flex-col items-center shrink-0">
                      <div class="w-3 h-3 rounded-full {isHead ? 'bg-success ring-2 ring-success/30' : 'bg-accent'}"></div>
                      {#if i < commitsArray.length - 1 || hasMoreCommits}
                        <div class="w-0.5 flex-1 bg-surface-3 mt-1"></div>
                      {/if}
                    </div>

                    <!-- Commit info -->
                    <div class="flex-1 min-w-0">
                      <div class="flex items-center gap-2 mb-1">
                        <span class="font-medium text-text-1 truncate" title={commit.message}>
                          {getCommitTitle(commit.message)}
                        </span>
                        {#if ciStatus?.status}
                          <button
                            class="btn-circle btn-ghost"
                            onclick={(event) => handleCIClick(commit.oid, event)}
                            title="View CI runs"
                          >
                            <CIStatusBadge status={ciStatus.status} compact />
                          </button>
                        {/if}
                        {#if isHead}
                          <span class="shrink-0 text-xs font-medium px-1.5 py-0.5 rounded bg-success/20 text-success flex items-center gap-1">
                            <span class="i-lucide-circle-dot text-[10px]"></span>
                            HEAD
                          </span>
                        {/if}
                      </div>
                      <div class="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm text-text-3">
                        {#if route.npub && repoName}
                          <a
                            href="#/{route.npub}/{repoName}?commit={commit.oid}"
                            onclick={close}
                            class="font-mono bg-surface-2 px-1.5 py-0.5 rounded text-xs hover:bg-accent hover:text-white transition-colors"
                            title="View commit diff"
                          >
                            {shortHash(commit.oid)}
                          </a>
                        {:else}
                          <span class="font-mono bg-surface-2 px-1.5 py-0.5 rounded text-xs">
                            {shortHash(commit.oid)}
                          </span>
                        {/if}
                        <span class="flex items-center gap-1">
                          <span class="i-lucide-user text-xs"></span>
                          {commit.author}
                        </span>
                        <span class="flex items-center gap-1">
                          <span class="i-lucide-clock text-xs"></span>
                          {formatDate(commit.timestamp)}
                        </span>
                      </div>
                    </div>

                    <!-- Action button -->
                    <div class="shrink-0">
                      {#if target.canEdit && target.onCheckout}
                        {#if isHead}
                          <span class="text-xs text-text-3 px-2 py-1">Current</span>
                        {:else}
                          <button
                            onclick={() => handleCheckout(commit.oid)}
                            disabled={checkoutInProgress !== null}
                            class="btn-ghost px-2 py-1 text-xs flex items-center gap-1"
                            title="Checkout this commit (replaces working directory)"
                          >
                            {#if checkoutInProgress === commit.oid}
                              <span class="i-lucide-loader-2 animate-spin"></span>
                            {:else}
                              <span class="i-lucide-git-branch-plus"></span>
                            {/if}
                            Checkout
                          </button>
                        {/if}
                      {:else}
                        <button
                          onclick={() => handleBrowse(commit.oid)}
                          class="btn-ghost px-2 py-1 text-xs flex items-center gap-1"
                          title="Browse files at this commit"
                        >
                          <span class="i-lucide-external-link"></span>
                          Browse
                        </button>
                      {/if}
                    </div>
                  </div>
                {/each}

                <!-- Loading indicator -->
                {#if loadingMore}
                  <div class="flex justify-center py-4">
                    <span class="i-lucide-loader-2 animate-spin text-text-3"></span>
                  </div>
                {/if}
              </div>
          </InfiniteScroll>
        {/if}
      </div>

      <!-- Footer -->
      <div class="flex justify-between items-center p-4 b-t-1 b-t-solid b-t-surface-3">
        <span class="text-sm text-text-3">
          {uniqueCommits().length} commit{uniqueCommits().length !== 1 ? 's' : ''}
        </span>
        <button onclick={close} class="btn-ghost">Close</button>
      </div>
    </div>
  </div>
{/if}
