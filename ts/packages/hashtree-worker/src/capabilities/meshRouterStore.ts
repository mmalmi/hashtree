import type { Hash, Store } from '@hashtree/core';
import { toHex } from '@hashtree/core';
import {
  buildHedgedWavePlan,
  normalizeDispatchConfig,
  type RequestDispatchConfig,
} from '@hashtree/nostr';

const DEFAULT_DISPATCH: RequestDispatchConfig = {
  initialFanout: 1,
  hedgeFanout: 1,
  maxFanout: 4,
  hedgeIntervalMs: 75,
};

const DEFAULT_REQUEST_TIMEOUT_MS = 5_500;
const DEFAULT_PRIMARY_READ_TIMEOUT_MS = 300;
const INITIAL_BACKOFF_MS = 250;
const MAX_BACKOFF_MS = 10_000;
const SCORE_TIE_DELTA = 0.15;

export interface MeshReadSource {
  id: string;
  get(hash: Hash): Promise<Uint8Array | null>;
  isAvailable?: () => boolean;
}

export interface MeshRouterGetOptions {
  sourceIds?: readonly string[];
  skipPrimary?: boolean;
}

export interface MeshRouterGetResult {
  data: Uint8Array;
  sourceId: string;
}

export interface MeshRouterStoreConfig {
  primary: Store;
  sources?: MeshReadSource[];
  dispatch?: RequestDispatchConfig;
  requestTimeoutMs?: number;
  primaryReadTimeoutMs?: number;
  primarySourceId?: string;
}

interface SourceStats {
  requests: number;
  successes: number;
  misses: number;
  failures: number;
  timeouts: number;
  srttMs: number;
  rttvarMs: number;
  backoffLevel: number;
  backedOffUntilMs?: number;
  lastSuccessMs?: number;
  lastFailureMs?: number;
}

interface InFlightSourceRequest {
  source: MeshReadSource;
  settled: boolean;
  timeoutRecorded: boolean;
  promise: Promise<{ sourceId: string; data: Uint8Array | null }>;
}

function defaultStats(): SourceStats {
  return {
    requests: 0,
    successes: 0,
    misses: 0,
    failures: 0,
    timeouts: 0,
    srttMs: 0,
    rttvarMs: 0,
    backoffLevel: 0,
  };
}

function reliabilityScore(stats: SourceStats): number {
  return (stats.successes + 1) / (stats.requests + 2);
}

function latencyScore(stats: SourceStats): number {
  if (stats.srttMs <= 0) return 0.5;
  return Math.min(1, 500 / (stats.srttMs + 50));
}

function hasHistory(stats: SourceStats): boolean {
  return stats.requests > 0 || stats.successes > 0 || stats.misses > 0 || stats.failures > 0 || stats.timeouts > 0;
}

function scoreSource(stats: SourceStats, now: number): number {
  if (stats.backedOffUntilMs && stats.backedOffUntilMs > now) {
    return Number.NEGATIVE_INFINITY;
  }

  const missPenalty = stats.requests > 0 ? (stats.misses / stats.requests) * 0.15 : 0;
  const failurePenalty = stats.requests > 0 ? ((stats.failures + stats.timeouts) / stats.requests) * 0.3 : 0;
  const recencyBonus =
    stats.lastSuccessMs && now - stats.lastSuccessMs < 60_000
      ? 0.1
      : 0;

  return (
    0.6 * reliabilityScore(stats) +
    0.3 * latencyScore(stats) +
    recencyBonus -
    missPenalty -
    failurePenalty
  );
}

export class MeshRouterStore implements Store {
  private readonly primary: Store;
  private readonly primarySourceId: string;
  private readonly dispatch: RequestDispatchConfig;
  private readonly requestTimeoutMs: number;
  private readonly primaryReadTimeoutMs: number;
  private readonly sources = new Map<string, MeshReadSource>();
  private readonly statsBySource = new Map<string, SourceStats>();
  private readonly inflightReads = new Map<string, Promise<MeshRouterGetResult | null>>();

  constructor(config: MeshRouterStoreConfig) {
    this.primary = config.primary;
    this.primarySourceId = config.primarySourceId ?? 'primary';
    this.dispatch = config.dispatch ?? DEFAULT_DISPATCH;
    this.requestTimeoutMs = config.requestTimeoutMs ?? DEFAULT_REQUEST_TIMEOUT_MS;
    this.primaryReadTimeoutMs = config.primaryReadTimeoutMs ?? DEFAULT_PRIMARY_READ_TIMEOUT_MS;
    this.setSources(config.sources ?? []);
  }

  setSources(sources: MeshReadSource[]): void {
    this.sources.clear();
    for (const source of sources) {
      this.sources.set(source.id, source);
      this.statsBySource.set(source.id, this.statsBySource.get(source.id) ?? defaultStats());
    }
  }

  addSource(source: MeshReadSource): void {
    this.sources.set(source.id, source);
    this.statsBySource.set(source.id, this.statsBySource.get(source.id) ?? defaultStats());
  }

  removeSource(sourceId: string): void {
    this.sources.delete(sourceId);
  }

  async getDetailed(hash: Hash, options: MeshRouterGetOptions = {}): Promise<MeshRouterGetResult | null> {
    if (!options.skipPrimary) {
      const local = await this.readPrimary(hash);
      if (local) {
        return { data: local, sourceId: this.primarySourceId };
      }
    }

    const pendingKey = this.pendingReadKey(hash, options);
    let pending = this.inflightReads.get(pendingKey);
    if (!pending) {
      pending = this.loadFromSources(hash, options).finally(() => {
        if (this.inflightReads.get(pendingKey) === pending) {
          this.inflightReads.delete(pendingKey);
        }
      });
      this.inflightReads.set(pendingKey, pending);
    }

    return pending;
  }

  getSourceStats(): Record<string, SourceStats> {
    return Object.fromEntries(
      Array.from(this.statsBySource.entries()).map(([sourceId, stats]) => [sourceId, { ...stats }]),
    );
  }

  async put(hash: Hash, data: Uint8Array): Promise<boolean> {
    return this.primary.put(hash, data);
  }

  async get(hash: Hash): Promise<Uint8Array | null> {
    return (await this.getDetailed(hash))?.data ?? null;
  }

  async has(hash: Hash): Promise<boolean> {
    return this.primary.has(hash);
  }

  async delete(hash: Hash): Promise<boolean> {
    return this.primary.delete(hash);
  }

  private async readPrimary(hash: Hash): Promise<Uint8Array | null> {
    if (this.primaryReadTimeoutMs <= 0) {
      return await this.primary.get(hash);
    }

    return await new Promise<Uint8Array | null>((resolve) => {
      let settled = false;
      const timeoutId = setTimeout(() => {
        if (settled) return;
        settled = true;
        resolve(null);
      }, this.primaryReadTimeoutMs);

      this.primary.get(hash)
        .then((data) => {
          if (settled) return;
          settled = true;
          clearTimeout(timeoutId);
          resolve(data);
        })
        .catch(() => {
          if (settled) return;
          settled = true;
          clearTimeout(timeoutId);
          resolve(null);
        });
    });
  }

  private pendingReadKey(hash: Hash, options: MeshRouterGetOptions): string {
    const sourceKey = options.sourceIds && options.sourceIds.length > 0
      ? [...options.sourceIds].sort().join(',')
      : '*';
    return `${toHex(hash)}:${options.skipPrimary === true ? 'skip-primary' : 'with-primary'}:${sourceKey}`;
  }

  private getCandidateSources(sourceIds?: readonly string[]): MeshReadSource[] {
    const requested = sourceIds && sourceIds.length > 0
      ? new Set(sourceIds)
      : null;
    const available = Array.from(this.sources.values()).filter((source) => {
      if (requested && !requested.has(source.id)) return false;
      return source.isAvailable ? source.isAvailable() : true;
    });
    if (available.length === 0) return [];

    const now = Date.now();
    const healthy = available.filter((source) => {
      const stats = this.statsBySource.get(source.id) ?? defaultStats();
      return !stats.backedOffUntilMs || stats.backedOffUntilMs <= now;
    });

    return healthy.length > 0 ? healthy : available;
  }

  private orderedSources(sourceIds?: readonly string[]): MeshReadSource[] {
    const now = Date.now();
    const candidates = this.getCandidateSources(sourceIds);
    return candidates.sort((left, right) => {
      const leftStats = this.statsBySource.get(left.id) ?? defaultStats();
      const rightStats = this.statsBySource.get(right.id) ?? defaultStats();
      const scoreDiff = scoreSource(rightStats, now) - scoreSource(leftStats, now);
      if (scoreDiff !== 0) return scoreDiff;
      return left.id.localeCompare(right.id);
    });
  }

  private shouldProbeMultipleSources(orderedSources: MeshReadSource[]): boolean {
    if (orderedSources.length <= 1) return false;

    const [best, secondBest] = orderedSources;
    const bestStats = this.statsBySource.get(best.id) ?? defaultStats();
    const secondStats = this.statsBySource.get(secondBest.id) ?? defaultStats();
    if (!hasHistory(bestStats) || !hasHistory(secondStats)) {
      return true;
    }

    const now = Date.now();
    const diff = scoreSource(bestStats, now) - scoreSource(secondStats, now);
    return diff < SCORE_TIE_DELTA;
  }

  private dispatchFor(sourceCount: number, orderedSources: MeshReadSource[]): RequestDispatchConfig {
    const probeMultiple = this.shouldProbeMultipleSources(orderedSources);
    const initialFanout = probeMultiple
      ? Math.min(sourceCount, 2)
      : 1;
    return {
      initialFanout,
      hedgeFanout: this.dispatch.hedgeFanout,
      maxFanout: Math.min(this.dispatch.maxFanout, sourceCount),
      hedgeIntervalMs: this.dispatch.hedgeIntervalMs,
    };
  }

  private createInFlightSourceRequest(source: MeshReadSource, hash: Hash): InFlightSourceRequest {
    const startedAt = Date.now();
    this.recordRequest(source.id);

    const task: InFlightSourceRequest = {
      source,
      settled: false,
      timeoutRecorded: false,
      promise: Promise.resolve({ sourceId: source.id, data: null }),
    };

    task.promise = source.get(hash)
      .then(async (data) => {
        const elapsedMs = Math.max(1, Date.now() - startedAt);
        if (data) {
          const stableData = data.slice();
          this.recordSuccess(source.id, elapsedMs);
          await this.primary.put(hash, stableData).catch(() => false);
          return { sourceId: source.id, data: stableData };
        }

        if (!task.timeoutRecorded) {
          this.recordMiss(source.id);
        }
        return { sourceId: source.id, data: null };
      })
      .catch(() => {
        if (!task.timeoutRecorded) {
          this.recordFailure(source.id);
        }
        return { sourceId: source.id, data: null };
      });

    return task;
  }

  private async waitForNextResult(
    inFlight: InFlightSourceRequest[],
    waitMs?: number,
  ): Promise<{ task: InFlightSourceRequest; sourceId: string; data: Uint8Array | null } | null> {
    const active = inFlight.filter((task) => !task.settled);
    if (active.length === 0) return null;
    if (waitMs !== undefined && waitMs <= 0) return null;

    const outcomes = active.map((task) => task.promise.then((value) => ({ task, ...value })));
    const result = waitMs === undefined
      ? await Promise.race(outcomes)
      : await Promise.race([
        new Promise<null>((resolve) => {
          setTimeout(() => resolve(null), waitMs);
        }),
        ...outcomes,
      ]);
    if (!result) return null;

    result.task.settled = true;
    return result;
  }

  private async loadFromSources(hash: Hash, options: MeshRouterGetOptions): Promise<MeshRouterGetResult | null> {
    const orderedSources = this.orderedSources(options.sourceIds);
    if (orderedSources.length === 0) {
      return null;
    }

    const dispatch = normalizeDispatchConfig(
      this.dispatchFor(orderedSources.length, orderedSources),
      orderedSources.length,
    );
    const wavePlan = buildHedgedWavePlan(orderedSources.length, dispatch);
    if (wavePlan.length === 0) return null;

    const inFlight: InFlightSourceRequest[] = [];
    let nextSourceIdx = 0;

    for (let waveIdx = 0; waveIdx < wavePlan.length; waveIdx++) {
      const waveSize = wavePlan[waveIdx];
      const from = nextSourceIdx;
      const to = Math.min(from + waveSize, orderedSources.length);
      nextSourceIdx = to;

      for (const source of orderedSources.slice(from, to)) {
        inFlight.push(this.createInFlightSourceRequest(source, hash));
      }

      const isLastWave = waveIdx === wavePlan.length - 1 || nextSourceIdx >= orderedSources.length;
      const windowEnd = isLastWave ? null : Date.now() + dispatch.hedgeIntervalMs;

      while (isLastWave || Date.now() < (windowEnd ?? 0)) {
        const remaining = windowEnd === null ? undefined : windowEnd - Date.now();
        const result = await this.waitForNextResult(inFlight, remaining);
        if (!result) break;
        if (result.data) {
          return {
            data: result.data,
            sourceId: result.sourceId,
          };
        }
      }
    }

    for (const task of inFlight) {
      if (task.settled) continue;
      task.timeoutRecorded = true;
      this.recordTimeout(task.source.id);
    }

    return null;
  }

  private statsFor(sourceId: string): SourceStats {
    const stats = this.statsBySource.get(sourceId);
    if (stats) return stats;
    const created = defaultStats();
    this.statsBySource.set(sourceId, created);
    return created;
  }

  private recordRequest(sourceId: string): void {
    this.statsFor(sourceId).requests += 1;
  }

  private recordMiss(sourceId: string): void {
    this.statsFor(sourceId).misses += 1;
  }

  private recordSuccess(sourceId: string, elapsedMs: number): void {
    const stats = this.statsFor(sourceId);
    const now = Date.now();
    stats.successes += 1;
    stats.lastSuccessMs = now;
    stats.backoffLevel = 0;
    stats.backedOffUntilMs = undefined;

    if (stats.srttMs === 0) {
      stats.srttMs = elapsedMs;
      stats.rttvarMs = elapsedMs / 2;
      return;
    }

    stats.rttvarMs = 0.75 * stats.rttvarMs + 0.25 * Math.abs(stats.srttMs - elapsedMs);
    stats.srttMs = 0.875 * stats.srttMs + 0.125 * elapsedMs;
  }

  private recordFailure(sourceId: string): void {
    const stats = this.statsFor(sourceId);
    stats.failures += 1;
    stats.lastFailureMs = Date.now();
    this.applyBackoff(stats);
  }

  private recordTimeout(sourceId: string): void {
    const stats = this.statsFor(sourceId);
    stats.timeouts += 1;
    stats.lastFailureMs = Date.now();
    this.applyBackoff(stats);
  }

  private applyBackoff(stats: SourceStats): void {
    stats.backoffLevel += 1;
    const backoffMs = Math.min(
      MAX_BACKOFF_MS,
      INITIAL_BACKOFF_MS * (2 ** Math.max(0, stats.backoffLevel - 1)),
    );
    stats.backedOffUntilMs = Date.now() + backoffMs;
  }
}
