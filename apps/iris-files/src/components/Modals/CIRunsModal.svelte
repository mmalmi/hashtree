<script lang="ts" module>
  import type { CIStatus } from '../../stores/ci';
  import type { Readable } from 'svelte/store';

  export interface CIRunsTarget {
    status: CIStatus;
    repoPath: string;
    statusStore?: Readable<CIStatus>;
  }

  let show = $state(false);
  let target = $state<CIRunsTarget | null>(null);
  let status = $state<CIStatus | null>(null);
  let statusUnsub: (() => void) | null = null;

  export function open(t: CIRunsTarget) {
    if (statusUnsub) {
      statusUnsub();
      statusUnsub = null;
    }
    target = t;
    status = t.status;
    if (t.statusStore) {
      statusUnsub = t.statusStore.subscribe(value => {
        status = value;
      });
    }
    show = true;
  }

  export function close() {
    show = false;
    target = null;
    status = null;
    if (statusUnsub) {
      statusUnsub();
      statusUnsub = null;
    }
  }
</script>

<script lang="ts">
  import CIStatusBadge from '../Git/CIStatusBadge.svelte';
  import { getWorkerAdapter, waitForWorkerAdapter } from '../../lib/workerInit';
  import { getRefResolver } from '../../refResolver';
  import { getLocalRootCache, getLocalRootKey } from '../../treeRootCache';
  import { toHex, type CID } from 'hashtree';

  let selectedJobIndex = $state(0);
  let selectedStepIndex = $state(0);
  let logText = $state('');
  let logLoading = $state(false);
  let logError = $state<string | null>(null);
  let logRequestId = 0;
  const RESOLVE_TIMEOUT_MS = 8000;

  let jobs = $derived(status?.jobs ?? []);
  let selectedJob = $derived(jobs[selectedJobIndex] ?? null);
  let steps = $derived(selectedJob?.steps ?? []);
  let selectedStep = $derived(steps[selectedStepIndex] ?? null);

  let selectedLogHash = $derived.by(() => {
    if (selectedStep?.logs_hash) return selectedStep.logs_hash;
    if (selectedJob?.logs_hash) return selectedJob.logs_hash;
    return '';
  });

  $effect(() => {
    if (!show) {
      selectedJobIndex = 0;
      selectedStepIndex = 0;
      logText = '';
      logLoading = false;
      logError = null;
      return;
    }
    if (selectedJobIndex >= jobs.length) {
      selectedJobIndex = 0;
    }
  });

  let lastJobId = $state<string | null>(null);
  $effect(() => {
    const jobId = selectedJob?.job_id ?? null;
    if (jobId !== lastJobId) {
      selectedStepIndex = 0;
      lastJobId = jobId;
    }
  });

  $effect(() => {
    if (!show) return;
    if (selectedStepIndex >= steps.length) {
      selectedStepIndex = 0;
    }
  });

  $effect(() => {
    logText = '';
    logError = null;
    if (!show) return;
    const hash = selectedLogHash;
    if (!hash) return;
    logLoading = true;
    const requestId = ++logRequestId;

    fetchLog(hash).then(text => {
      if (requestId !== logRequestId) return;
      logText = text;
      logLoading = false;
    }).catch((err) => {
      if (requestId !== logRequestId) return;
      logError = err instanceof Error ? err.message : String(err);
      logLoading = false;
    });
  });

  function normalizeLogKey(rawHash: string): string {
    const trimmed = rawHash.trim();
    if (!trimmed) return '';
    try {
      if (trimmed.includes('://')) {
        const url = new URL(trimmed);
        const lastSegment = url.pathname.split('/').filter(Boolean).pop() || '';
        return lastSegment.replace(/\.bin$/, '');
      }
    } catch {
      // Fall through to string parsing
    }
    const withoutScheme = trimmed.replace(/^blossom:\/\//, '');
    const pathParts = withoutScheme.split('/').filter(Boolean);
    const lastPart = pathParts.length > 0 ? pathParts[pathParts.length - 1] : withoutScheme;
    const colonParts = lastPart.split(':');
    const candidate = colonParts[colonParts.length - 1] || '';
    const cleaned = candidate.replace(/\.bin$/, '');
    if (/^[0-9a-fA-F]+$/.test(cleaned)) return cleaned.toLowerCase();
    return cleaned;
  }

  function normalizeStepName(stepName: string): string {
    return stepName
      .split('')
      .map((char) => (/[A-Za-z0-9_-]/.test(char) ? char : '_'))
      .join('');
  }

  function buildRepoPathCandidates(repoPath: string): string[][] {
    const parts = repoPath.split('/').filter(Boolean);
    if (parts.length === 0) return [];
    const candidates: string[][] = [];
    const seen: string[] = [];
    const add = (segments: string[]) => {
      if (segments.length === 0) return;
      const key = segments.join('\u0000');
      if (seen.includes(key)) return;
      seen.push(key);
      candidates.push(segments);
    };

    add(parts);
    if (parts.length > 1) {
      add([parts.join('/')]);
      const withoutFirst = parts.slice(1);
      add(withoutFirst);
      if (withoutFirst.length > 1) {
        add([withoutFirst.join('/')]);
      }
      add([parts[parts.length - 1]]);
    }

    return candidates;
  }

  async function resolveRunnerRoot(runnerNpub: string, treeName: string): Promise<CID | null> {
    const adapter = getWorkerAdapter();
    if (adapter) {
      const cached = await adapter.resolveRoot(runnerNpub, treeName);
      if (cached) return cached;
    }

    const localHash = getLocalRootCache(runnerNpub, treeName);
    if (localHash) {
      return { hash: localHash, key: getLocalRootKey(runnerNpub, treeName) };
    }

    const resolver = getRefResolver();
    if (!resolver.resolve) return null;

    const key = `${runnerNpub}/${treeName}`;
    const resolved = await Promise.race([
      resolver.resolve(key),
      new Promise<null>((resolve) => setTimeout(() => resolve(null), RESOLVE_TIMEOUT_MS)),
    ]);
    return resolved ?? null;
  }

  async function resolveDirPath(baseCid: CID, pathParts: string[]): Promise<CID | null> {
    const adapter = getWorkerAdapter() ?? await waitForWorkerAdapter();
    if (!adapter) return null;

    let currentCid = baseCid;
    for (const part of pathParts) {
      const entries = await adapter.listDir(currentCid);
      const entry = entries.find(e => e.name === part);
      if (!entry?.cid || !entry.isDir) return null;
      currentCid = entry.cid;
    }
    return currentCid;
  }

  async function fetchLogFromTree(logKeyHex: string): Promise<string | null> {
    if (!target?.repoPath || !selectedJob?.runner_npub || !selectedJob?.commit) return null;
    const adapter = getWorkerAdapter() ?? await waitForWorkerAdapter();
    if (!adapter) return null;

    const rootCid = await resolveRunnerRoot(selectedJob.runner_npub, 'ci');
    if (!rootCid) return null;

    const repoPathCandidates = buildRepoPathCandidates(target.repoPath);
    if (repoPathCandidates.length === 0) return null;
    const commitCandidates = Array.from(new Set([selectedJob.commit, selectedJob.commit.slice(0, 8)].filter(Boolean)));

    for (const commitPart of commitCandidates) {
      for (const repoParts of repoPathCandidates) {
        const commitDir = await resolveDirPath(rootCid, [...repoParts, commitPart]);
        if (!commitDir) continue;
        const entries = await adapter.listDir(commitDir);
        const logsEntry = entries.find(entry => entry.name === 'logs' && entry.isDir);
        if (!logsEntry?.cid) continue;
        const logEntries = await adapter.listDir(logsEntry.cid);
        const expectedNames: string[] = [];
        const addExpectedName = (name: string) => {
          if (!expectedNames.includes(name)) expectedNames.push(name);
        };
        if (selectedStep?.name) {
          const rawName = selectedStep.name;
          const baseName = rawName.endsWith('.txt') ? rawName.slice(0, -4) : rawName;
          addExpectedName(`${baseName}.txt`);
          const normalized = normalizeStepName(baseName);
          addExpectedName(`${normalized}.txt`);
        }
        const match = logEntries.find(entry => {
          if (!entry.cid) return false;
          if (logKeyHex && entry.cid.key && toHex(entry.cid.key) === logKeyHex) return true;
          if (logKeyHex && toHex(entry.cid.hash) === logKeyHex) return true;
          if (expectedNames.length > 0 && expectedNames.includes(entry.name)) return true;
          return false;
        });
        if (!match?.cid) continue;
        const data = await adapter.readFile(match.cid);
        return new TextDecoder().decode(data);
      }
    }

    return null;
  }

  async function fetchLog(rawHash: string): Promise<string> {
    const logKey = normalizeLogKey(rawHash);
    const treeLog = await fetchLogFromTree(logKey);
    if (treeLog !== null) return treeLog;

    if (!logKey) {
      throw new Error('Log hash is missing');
    }
    throw new Error('Log not found in CI logs directory');
  }

  function formatDuration(seconds: number): string {
    if (!Number.isFinite(seconds) || seconds <= 0) return '0s';
    const totalSeconds = Math.round(seconds);
    if (totalSeconds < 60) return `${totalSeconds}s`;
    const minutes = Math.floor(totalSeconds / 60);
    const secs = totalSeconds % 60;
    if (minutes < 60) return `${minutes}m ${secs}s`;
    const hours = Math.floor(minutes / 60);
    const mins = minutes % 60;
    return `${hours}h ${mins}m`;
  }

  function shortCommit(commit: string | undefined): string {
    if (!commit) return '';
    return commit.length > 8 ? commit.slice(0, 8) : commit;
  }

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
</script>

{#if show && target}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div
    class="fixed inset-0 bg-black/70 flex-center z-1000 overflow-auto"
    onclick={(e) => {
      if (e.target === e.currentTarget) close();
    }}
    data-testid="ci-runs-modal-backdrop"
  >
    <div class="bg-surface-1 sm:rounded-lg overflow-hidden w-screen sm:w-[96vw] sm:max-w-6xl b-1 b-solid b-surface-3 max-h-[90vh] flex flex-col">
      <div class="p-4 b-b-1 b-b-solid b-b-surface-3 flex items-center justify-between gap-3">
        <div class="flex flex-col gap-1 min-w-0">
          <h2 class="text-lg font-semibold flex items-center gap-2">
            <span class="i-lucide-activity"></span>
            CI Runs
          </h2>
          {#if target.repoPath}
            <div class="text-xs text-text-3 truncate">{target.repoPath}</div>
          {/if}
        </div>
        <button onclick={close} class="btn-circle btn-ghost" aria-label="Close">
          <span class="i-lucide-x text-lg"></span>
        </button>
      </div>

      {#if jobs.length === 0}
        <div class="p-6 text-text-3 text-sm">No CI runs found for this commit.</div>
      {:else}
        <div class="flex-1 flex flex-col md:flex-row min-h-0">
          <div class="w-full md:w-60 b-b-1 md:b-b-0 md:b-r-1 b-b-solid md:b-r-solid b-surface-3 overflow-auto">
            <div class="p-3 text-xs text-text-3 uppercase tracking-wide">Jobs</div>
            {#each jobs as job, index (job.job_id)}
              <button
                class="btn-ghost w-full rounded-none text-left px-3 py-2 flex flex-col gap-1 b-b-1 b-b-solid b-b-surface-3 {index === selectedJobIndex ? 'bg-surface-2' : ''}"
                onclick={() => selectedJobIndex = index}
              >
                <div class="flex items-center justify-between gap-2">
                  <span class="truncate text-sm">{job.job_name}</span>
                  <CIStatusBadge status={job.status} compact />
                </div>
                <div class="text-xs text-text-3 truncate">{job.workflow}</div>
                <div class="text-[11px] text-text-3 truncate">
                  {shortCommit(job.commit)}
                </div>
              </button>
            {/each}
          </div>

          <div class="w-full md:w-72 b-b-1 md:b-b-0 md:b-r-1 b-b-solid md:b-r-solid b-surface-3 overflow-auto">
            <div class="p-3 text-xs text-text-3 uppercase tracking-wide">Steps</div>
            {#if steps.length === 0}
              <div class="p-3 text-text-3 text-sm">No steps recorded.</div>
            {:else}
              {#each steps as step, stepIndex (step.name)}
                <button
                  class="btn-ghost w-full rounded-none text-left px-3 py-2 flex flex-col gap-1 b-b-1 b-b-solid b-b-surface-3 {stepIndex === selectedStepIndex ? 'bg-surface-2' : ''}"
                  onclick={() => selectedStepIndex = stepIndex}
                >
                  <div class="flex items-center justify-between gap-2">
                    <span class="truncate text-sm">{step.name}</span>
                    <CIStatusBadge status={step.status} compact />
                  </div>
                  <div class="text-xs text-text-3">{formatDuration(step.duration_secs)}</div>
                </button>
              {/each}
            {/if}
          </div>

          <div class="flex-1 min-h-0 flex flex-col">
            <div class="p-3 b-b-1 b-b-solid b-b-surface-3 flex items-center justify-between gap-2">
              <div class="text-sm font-medium truncate">
                {selectedStep ? selectedStep.name : 'Logs'}
              </div>
              {#if selectedJob}
                <div class="text-xs text-text-3 truncate">{selectedJob.job_name}</div>
              {/if}
            </div>
            <div class="flex-1 overflow-auto p-3">
              {#if logLoading}
                <div class="text-text-3 text-sm">Loading logs...</div>
              {:else if logError}
                <div class="text-danger text-sm">{logError}</div>
              {:else if logText}
                <pre class="text-xs whitespace-pre-wrap font-mono">{logText}</pre>
              {:else}
                <div class="text-text-3 text-sm">No logs available.</div>
              {/if}
            </div>
          </div>
        </div>
      {/if}
    </div>
  </div>
{/if}
