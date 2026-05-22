import { toHex } from '@hashtree/core';
import { buildHedgedWavePlan, normalizeDispatchConfig, } from '@hashtree/mesh';
const DEFAULT_DISPATCH = {
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
function defaultStats() {
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
function reliabilityScore(stats) {
    return (stats.successes + 1) / (stats.requests + 2);
}
function latencyScore(stats) {
    if (stats.srttMs <= 0)
        return 0.5;
    return Math.min(1, 500 / (stats.srttMs + 50));
}
function hasHistory(stats) {
    return stats.requests > 0 || stats.successes > 0 || stats.misses > 0 || stats.failures > 0 || stats.timeouts > 0;
}
function scoreSource(stats, now) {
    if (stats.backedOffUntilMs && stats.backedOffUntilMs > now) {
        return Number.NEGATIVE_INFINITY;
    }
    const missPenalty = stats.requests > 0 ? (stats.misses / stats.requests) * 0.15 : 0;
    const failurePenalty = stats.requests > 0 ? ((stats.failures + stats.timeouts) / stats.requests) * 0.3 : 0;
    const recencyBonus = stats.lastSuccessMs && now - stats.lastSuccessMs < 60_000
        ? 0.1
        : 0;
    return (0.6 * reliabilityScore(stats) +
        0.3 * latencyScore(stats) +
        recencyBonus -
        missPenalty -
        failurePenalty);
}
export class MeshRouterStore {
    primary;
    primarySourceId;
    dispatch;
    requestTimeoutMs;
    primaryReadTimeoutMs;
    sources = new Map();
    sourceProviders;
    statsBySource = new Map();
    inflightReads = new Map();
    constructor(config) {
        this.primary = config.primary;
        this.primarySourceId = config.primarySourceId ?? 'primary';
        this.dispatch = config.dispatch ?? DEFAULT_DISPATCH;
        this.requestTimeoutMs = config.requestTimeoutMs ?? DEFAULT_REQUEST_TIMEOUT_MS;
        this.primaryReadTimeoutMs = config.primaryReadTimeoutMs ?? DEFAULT_PRIMARY_READ_TIMEOUT_MS;
        this.sourceProviders = [...(config.sourceProviders ?? [])];
        this.setSources(config.sources ?? []);
    }
    setSources(sources) {
        this.sources.clear();
        for (const source of sources) {
            this.sources.set(source.id, source);
            this.statsBySource.set(source.id, this.statsBySource.get(source.id) ?? defaultStats());
        }
    }
    addSource(source) {
        this.sources.set(source.id, source);
        this.statsBySource.set(source.id, this.statsBySource.get(source.id) ?? defaultStats());
    }
    removeSource(sourceId) {
        this.sources.delete(sourceId);
    }
    async getDetailed(hash, options = {}) {
        if (!options.skipPrimary) {
            const primary = this.readPrimary(hash);
            if (this.primaryReadTimeoutMs <= 0) {
                const local = await primary;
                if (local) {
                    return this.primaryResult(local);
                }
                return await this.loadFromSourcesShared(hash, options);
            }
            else {
                const localWindowResult = await Promise.race([
                    primary.then((data) => ({ kind: 'primary', data })),
                    new Promise((resolve) => {
                        setTimeout(() => resolve({ kind: 'timeout' }), this.primaryReadTimeoutMs);
                    }),
                ]);
                if (localWindowResult.kind === 'primary') {
                    if (localWindowResult.data) {
                        return this.primaryResult(localWindowResult.data);
                    }
                    return await this.loadFromSourcesShared(hash, options);
                }
                const remotePromise = this.loadFromSourcesShared(hash, options);
                const firstResolved = await Promise.race([
                    primary.then((data) => ({
                        source: 'primary',
                        result: data ? this.primaryResult(data) : null,
                    })),
                    remotePromise.then((result) => ({
                        source: 'remote',
                        result,
                    })),
                ]);
                if (firstResolved.result) {
                    return firstResolved.result;
                }
                if (firstResolved.source === 'primary') {
                    return await remotePromise;
                }
                const eventualPrimary = await primary;
                return eventualPrimary ? this.primaryResult(eventualPrimary) : null;
            }
        }
        return await this.loadFromSourcesShared(hash, options);
    }
    loadFromSourcesShared(hash, options) {
        const pendingKey = this.pendingReadKey(hash, { ...options, skipPrimary: true });
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
    getSourceStats() {
        return Object.fromEntries(Array.from(this.statsBySource.entries()).map(([sourceId, stats]) => [sourceId, { ...stats }]));
    }
    async put(hash, data) {
        return this.primary.put(hash, data);
    }
    async get(hash) {
        return (await this.getDetailed(hash))?.data ?? null;
    }
    async has(hash) {
        return this.primary.has(hash);
    }
    async delete(hash) {
        return this.primary.delete(hash);
    }
    async readPrimary(hash) {
        try {
            return await this.primary.get(hash);
        }
        catch {
            return null;
        }
    }
    primaryResult(data) {
        return {
            data: data.slice(),
            sourceId: this.primarySourceId,
        };
    }
    pendingReadKey(hash, options) {
        const sourceKey = options.sourceIds && options.sourceIds.length > 0
            ? [...options.sourceIds].sort().join(',')
            : '*';
        return `${toHex(hash)}:${options.skipPrimary === true ? 'skip-primary' : 'with-primary'}:${sourceKey}`;
    }
    getCandidateSources(sourceIds) {
        const requested = sourceIds && sourceIds.length > 0
            ? new Set(sourceIds)
            : null;
        const combined = new Map();
        for (const provider of this.sourceProviders) {
            for (const source of provider()) {
                if (!combined.has(source.id)) {
                    combined.set(source.id, source);
                }
                this.statsBySource.set(source.id, this.statsBySource.get(source.id) ?? defaultStats());
            }
        }
        for (const source of this.sources.values()) {
            if (!combined.has(source.id)) {
                combined.set(source.id, source);
            }
        }
        const available = Array.from(combined.values()).filter((source) => {
            if (requested && !requested.has(source.id) && !requested.has(source.groupId ?? ''))
                return false;
            return source.isAvailable ? source.isAvailable() : true;
        });
        if (available.length === 0)
            return [];
        const now = Date.now();
        const healthy = available.filter((source) => {
            const stats = this.statsBySource.get(source.id) ?? defaultStats();
            return !stats.backedOffUntilMs || stats.backedOffUntilMs <= now;
        });
        return healthy.length > 0 ? healthy : available;
    }
    orderedSources(sourceIds) {
        const now = Date.now();
        const candidates = this.getCandidateSources(sourceIds);
        return candidates.sort((left, right) => {
            const leftStats = this.statsBySource.get(left.id) ?? defaultStats();
            const rightStats = this.statsBySource.get(right.id) ?? defaultStats();
            const scoreDiff = scoreSource(rightStats, now) - scoreSource(leftStats, now);
            if (scoreDiff !== 0)
                return scoreDiff;
            return left.id.localeCompare(right.id);
        });
    }
    shouldProbeMultipleSources(orderedSources) {
        if (orderedSources.length <= 1)
            return false;
        const [best, secondBest] = orderedSources;
        const bestStats = this.statsBySource.get(best.id) ?? defaultStats();
        const secondStats = this.statsBySource.get(secondBest.id) ?? defaultStats();
        if (!hasHistory(bestStats) || !hasHistory(secondStats)) {
            return false;
        }
        const now = Date.now();
        const diff = scoreSource(bestStats, now) - scoreSource(secondStats, now);
        return diff < SCORE_TIE_DELTA;
    }
    dispatchFor(sourceCount, orderedSources) {
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
    createInFlightSourceRequest(source, hash) {
        const startedAt = Date.now();
        this.recordRequest(source.id);
        const task = {
            source,
            settled: false,
            timeoutRecorded: false,
            promise: Promise.resolve({ sourceId: source.id, data: null }),
        };
        task.promise = new Promise((resolve) => {
            let completed = false;
            let timeoutId = null;
            const finish = (data) => {
                if (completed) {
                    return;
                }
                completed = true;
                if (timeoutId !== null) {
                    clearTimeout(timeoutId);
                }
                resolve({ sourceId: source.id, data });
            };
            if (Number.isFinite(this.requestTimeoutMs) && this.requestTimeoutMs > 0) {
                timeoutId = setTimeout(() => {
                    if (completed) {
                        return;
                    }
                    task.timeoutRecorded = true;
                    this.recordTimeout(source.id);
                    finish(null);
                }, this.requestTimeoutMs);
            }
            void source.get(hash)
                .then(async (data) => {
                if (completed) {
                    return;
                }
                const elapsedMs = Math.max(1, Date.now() - startedAt);
                if (data) {
                    const stableData = data.slice();
                    completed = true;
                    if (timeoutId !== null) {
                        clearTimeout(timeoutId);
                    }
                    this.recordSuccess(source.id, elapsedMs);
                    await this.primary.put(hash, stableData).catch(() => false);
                    resolve({ sourceId: source.id, data: stableData });
                    return;
                }
                if (!task.timeoutRecorded) {
                    this.recordMiss(source.id);
                }
                finish(null);
            })
                .catch(() => {
                if (completed) {
                    return;
                }
                if (!task.timeoutRecorded) {
                    this.recordFailure(source.id);
                }
                finish(null);
            });
        });
        return task;
    }
    sourceGroupKey(source) {
        return source.groupId ?? '';
    }
    hasPendingCrossGroupRequests(inFlight, nextSources) {
        if (nextSources.length === 0) {
            return false;
        }
        const nextGroups = new Set(nextSources.map((source) => this.sourceGroupKey(source)));
        return inFlight.some((task) => !task.settled && !nextGroups.has(this.sourceGroupKey(task.source)));
    }
    async waitForNextResult(inFlight, waitMs) {
        const active = inFlight.filter((task) => !task.settled);
        if (active.length === 0)
            return null;
        if (waitMs !== undefined && waitMs <= 0)
            return null;
        const outcomes = active.map((task) => task.promise.then((value) => ({ task, ...value })));
        const result = waitMs === undefined
            ? await Promise.race(outcomes)
            : await Promise.race([
                new Promise((resolve) => {
                    setTimeout(() => resolve(null), waitMs);
                }),
                ...outcomes,
            ]);
        if (!result)
            return null;
        result.task.settled = true;
        return result;
    }
    async loadFromSources(hash, options) {
        const orderedSources = this.orderedSources(options.sourceIds);
        if (orderedSources.length === 0) {
            return null;
        }
        const probeMultiple = this.shouldProbeMultipleSources(orderedSources);
        const dispatch = normalizeDispatchConfig(this.dispatchFor(orderedSources.length, orderedSources), orderedSources.length);
        const wavePlan = buildHedgedWavePlan(orderedSources.length, dispatch);
        if (wavePlan.length === 0)
            return null;
        const inFlight = [];
        let nextSourceIdx = 0;
        for (let waveIdx = 0; waveIdx < wavePlan.length; waveIdx++) {
            const waveSize = wavePlan[waveIdx];
            const from = nextSourceIdx;
            const to = Math.min(from + waveSize, orderedSources.length);
            const waveSources = orderedSources.slice(from, to);
            nextSourceIdx = to;
            if (!probeMultiple && this.hasPendingCrossGroupRequests(inFlight, waveSources)) {
                while (this.hasPendingCrossGroupRequests(inFlight, waveSources)) {
                    const result = await this.waitForNextResult(inFlight);
                    if (!result) {
                        break;
                    }
                    if (result.data) {
                        return {
                            data: result.data,
                            sourceId: result.sourceId,
                        };
                    }
                }
            }
            for (const source of waveSources) {
                inFlight.push(this.createInFlightSourceRequest(source, hash));
            }
            const isLastWave = waveIdx === wavePlan.length - 1 || nextSourceIdx >= orderedSources.length;
            const windowEnd = isLastWave ? null : Date.now() + dispatch.hedgeIntervalMs;
            while (isLastWave || Date.now() < (windowEnd ?? 0)) {
                const remaining = windowEnd === null ? undefined : windowEnd - Date.now();
                const result = await this.waitForNextResult(inFlight, remaining);
                if (!result)
                    break;
                if (result.data) {
                    return {
                        data: result.data,
                        sourceId: result.sourceId,
                    };
                }
            }
        }
        for (const task of inFlight) {
            if (task.settled)
                continue;
            task.timeoutRecorded = true;
            this.recordTimeout(task.source.id);
        }
        return null;
    }
    statsFor(sourceId) {
        const stats = this.statsBySource.get(sourceId);
        if (stats)
            return stats;
        const created = defaultStats();
        this.statsBySource.set(sourceId, created);
        return created;
    }
    recordRequest(sourceId) {
        this.statsFor(sourceId).requests += 1;
    }
    recordMiss(sourceId) {
        this.statsFor(sourceId).misses += 1;
    }
    recordSuccess(sourceId, elapsedMs) {
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
    recordFailure(sourceId) {
        const stats = this.statsFor(sourceId);
        stats.failures += 1;
        stats.lastFailureMs = Date.now();
        this.applyBackoff(stats);
    }
    recordTimeout(sourceId) {
        const stats = this.statsFor(sourceId);
        stats.timeouts += 1;
        stats.lastFailureMs = Date.now();
        this.applyBackoff(stats);
    }
    applyBackoff(stats) {
        stats.backoffLevel += 1;
        const backoffMs = Math.min(MAX_BACKOFF_MS, INITIAL_BACKOFF_MS * (2 ** Math.max(0, stats.backoffLevel - 1)));
        stats.backedOffUntilMs = Date.now() + backoffMs;
    }
}
//# sourceMappingURL=meshRouterStore.js.map