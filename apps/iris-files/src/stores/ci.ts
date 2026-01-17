/**
 * CI Status store for displaying CI results from hashtree
 *
 * CI results are stored at: npub1runner/ci/<repoPath>/<commit>/result.json
 */
import { writable, type Readable } from 'svelte/store';
import type { CID } from 'hashtree';
import { getWorkerAdapter, waitForWorkerAdapter } from '../lib/workerInit';
import { getRefResolver } from '../refResolver';
import { getLocalRootCache, getLocalRootKey, onCacheUpdate } from '../treeRootCache';

const RESOLVE_TIMEOUT_MS = 8000;

/** CI job status matching hashtree-ci's JobStatus enum */
export type CIJobStatus = 'queued' | 'running' | 'success' | 'failure' | 'cancelled' | 'skipped';

/** Result of a single CI step */
export interface CIStepResult {
  name: string;
  status: CIJobStatus;
  exit_code?: number;
  duration_secs: number;
  logs_hash: string;
  error?: string;
}

/** Complete CI job result (matches hashtree-ci's JobResult) */
export interface CIJobResult {
  job_id: string;
  runner_npub: string;
  repo_hash: string;
  commit: string;
  workflow: string;
  job_name: string;
  status: CIJobStatus;
  started_at: string;
  finished_at: string;
  logs_hash: string;
  artifacts_hash?: string;
  steps: CIStepResult[];
}

/** CI runner configured for display */
export interface TrustedRunner {
  npub: string;
  name?: string;
  tags?: string[];
}

/** CI configuration from .hashtree/ci.toml */
export interface CIConfig {
  org_npub?: string;
  runners: TrustedRunner[];
}

/** Aggregated CI status for a commit */
export interface CIStatus {
  /** Overall status (worst of all jobs) */
  status: CIJobStatus | null;
  /** Individual job results from all runners */
  jobs: CIJobResult[];
  /** Whether we're still loading */
  loading: boolean;
  /** Error message if loading failed */
  error: string | null;
}

/**
 * Determine overall status from multiple job statuses
 * Priority: failure > cancelled > running > queued > skipped > success
 */
function aggregateStatus(jobs: CIJobResult[]): CIJobStatus | null {
  if (jobs.length === 0) return null;

  const priorities: CIJobStatus[] = ['failure', 'cancelled', 'running', 'queued', 'skipped', 'success'];
  for (const status of priorities) {
    if (jobs.some(j => j.status === status)) {
      return status;
    }
  }
  return jobs[0].status;
}

function buildRepoPathCandidates(repoPath: string): string[][] {
  const parts = repoPath.split('/').filter(Boolean);
  if (parts.length === 0) return [];
  const candidates: string[][] = [];
  const seen = new Set<string>();
  const add = (segments: string[]) => {
    if (segments.length === 0) return;
    const key = segments.join('\u0000');
    if (seen.has(key)) return;
    seen.add(key);
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

function repoHashMatches(repoHash: string | undefined, repoPath: string): boolean {
  if (!repoHash) return true;
  if (repoHash === repoPath) return true;
  return repoHash.endsWith(`/${repoPath}`);
}

/**
 * Create a store for CI status of a specific commit
 *
 * @param repoPath - The repo path (e.g., "repos/myproject")
 * @param commit - The git commit SHA
 * @param trustedRunners - List of runner npubs to check
 */
export function createCIStatusStore(
  repoPath: string,
  commit: string,
  trustedRunners: TrustedRunner[]
): Readable<CIStatus> {
  const { subscribe, set } = writable<CIStatus>({
    status: null,
    jobs: [],
    loading: true,
    error: null,
  });

  if (!commit || trustedRunners.length === 0) {
    set({ status: null, jobs: [], loading: false, error: null });
    return { subscribe };
  }

  const MAX_ATTEMPTS = 3;
  const RETRY_DELAY_MS = 1500;
  let attempts = 0;
  let cacheUnsubscribe: (() => void) | null = null;
  let subscriberCount = 0;

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

  async function loadResultFromRunner(runnerNpub: string): Promise<CIJobResult | null> {
    try {
      const adapter = getWorkerAdapter() ?? await waitForWorkerAdapter();
      if (!adapter) return null;

      const repoPathCandidates = buildRepoPathCandidates(repoPath);
      if (repoPathCandidates.length === 0) return null;
      const commitCandidates = Array.from(new Set([commit, commit.slice(0, 8)].filter(Boolean)));

      const rootCid = await resolveRunnerRoot(runnerNpub, 'ci');
      if (!rootCid) return null;

      for (const commitPart of commitCandidates) {
        for (const repoParts of repoPathCandidates) {
          const dirCid = await resolveDirPath(rootCid, [...repoParts, commitPart]);
          if (!dirCid) continue;
          const entries = await adapter.listDir(dirCid);
          const resultEntry = entries.find(e => e.name === 'result.json');
          if (!resultEntry?.cid) continue;
          const data = await adapter.readFile(resultEntry.cid);
          const json = new TextDecoder().decode(data);
          const parsed = JSON.parse(json) as CIJobResult;
          const candidatePath = repoParts.join('/');
          if (!repoHashMatches(parsed.repo_hash, candidatePath)) continue;
          return parsed;
        }
      }
      return null;
    } catch {
      return null;
    }
  }

  // Fetch CI results from each configured runner
  async function loadStatus(): Promise<void> {
    attempts += 1;
    try {
      const results = await Promise.all(
        trustedRunners.map(async (runner) => loadResultFromRunner(runner.npub))
      );
      const jobs = results.filter((r): r is CIJobResult => r !== null);
      if (jobs.length === 0 && attempts < MAX_ATTEMPTS) {
        set({ status: null, jobs: [], loading: true, error: null });
        setTimeout(loadStatus, RETRY_DELAY_MS);
        return;
      }
      set({
        status: aggregateStatus(jobs),
        jobs,
        loading: false,
        error: null,
      });
    } catch (err) {
      if (attempts < MAX_ATTEMPTS) {
        set({ status: null, jobs: [], loading: true, error: null });
        setTimeout(loadStatus, RETRY_DELAY_MS);
        return;
      }
      set({
        status: null,
        jobs: [],
        loading: false,
        error: err instanceof Error ? err.message : 'Failed to load CI status',
      });
    }
  }

  const trustedRunnerSet = new Set(trustedRunners.map(runner => runner.npub));

  function handleCacheUpdate(npub: string, treeName: string) {
    if (treeName !== 'ci' || !trustedRunnerSet.has(npub)) return;
    attempts = 0;
    loadStatus();
  }

  loadStatus();

  return {
    subscribe(run) {
      const unsub = subscribe(run);
      subscriberCount += 1;
      if (subscriberCount === 1 && !cacheUnsubscribe) {
        cacheUnsubscribe = onCacheUpdate(handleCacheUpdate);
      }
      return () => {
        subscriberCount -= 1;
        if (subscriberCount === 0 && cacheUnsubscribe) {
          cacheUnsubscribe();
          cacheUnsubscribe = null;
        }
        unsub();
      };
    },
  };
}

/**
 * Parse CI config from .hashtree/ci.toml content
 */
export function parseCIConfig(tomlContent: string): CIConfig | null {
  try {
    // Simple TOML parsing for CI config
    // Format:
    // [ci]
    // org_npub = "npub1..."
    // [[ci.runners]]
    // npub = "npub1..."
    // name = "runner-1"

    const config: CIConfig = { runners: [] };
    const lines = tomlContent.split('\n');
    let currentRunner: Partial<TrustedRunner> | null = null;

    for (const line of lines) {
      const trimmed = line.trim();

      if (trimmed === '[[ci.runners]]') {
        if (currentRunner?.npub) {
          config.runners.push(currentRunner as TrustedRunner);
        }
        currentRunner = {};
        continue;
      }

      const match = trimmed.match(/^(\w+)\s*=\s*"([^"]+)"$/);
      if (match) {
        const [, key, value] = match;
        if (key === 'org_npub') {
          config.org_npub = value;
        } else if (currentRunner) {
          if (key === 'npub') currentRunner.npub = value;
          else if (key === 'name') currentRunner.name = value;
        }
      }
    }

    // Don't forget the last runner
    if (currentRunner?.npub) {
      config.runners.push(currentRunner as TrustedRunner);
    }

    return config;
  } catch {
    return null;
  }
}

/**
 * Load CI config from a repo's .hashtree/ci.toml
 */
export async function loadCIConfig(repoCid: CID): Promise<CIConfig | null> {
  try {
    const adapter = getWorkerAdapter() ?? await waitForWorkerAdapter();
    if (!adapter) return null;

    const normalizedCid: CID = {
      hash: new Uint8Array(repoCid.hash),
      key: repoCid.key ? new Uint8Array(repoCid.key) : undefined,
    };

    // List root to find .hashtree directory
    const rootEntries = await adapter.listDir(normalizedCid);
    const hashtreeDir = rootEntries.find(e => e.name === '.hashtree');
    if (!hashtreeDir) return null;

    // List .hashtree to find ci.toml
    const hashtreeEntries = await adapter.listDir(hashtreeDir.cid);
    const ciToml = hashtreeEntries.find(e => e.name === 'ci.toml');
    if (!ciToml) return null;

    // Read and parse ci.toml
    const data = await adapter.readFile(ciToml.cid);
    const content = new TextDecoder().decode(data);
    return parseCIConfig(content);
  } catch {
    return null;
  }
}
