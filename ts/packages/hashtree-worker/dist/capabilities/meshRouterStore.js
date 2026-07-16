import { BLOB_DEFAULT_HTL, createBlobRequest, sha256, toHex, } from '@hashtree/core';
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
function errorFrom(error) {
    return error instanceof Error ? error : new Error(String(error));
}
function combineErrors(errors) {
    if (errors.length === 1)
        return errors[0];
    return new AggregateError(errors, `Blob retrieval failed: ${errors.map((error) => error.message).join('; ')}`);
}
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
        const loadRemote = () => this.loadFromSourcesShared(hash, options);
        if (options.skipPrimary) {
            return this.finishRead(await loadRemote());
        }
        const primary = this.readPrimary(hash);
        if (this.primaryReadTimeoutMs <= 0) {
            const local = await primary;
            if (local.type === 'data')
                return local.result;
            return this.finishRead(local, await loadRemote());
        }
        const local = await Promise.race([
            primary,
            new Promise((resolve) => {
                setTimeout(() => resolve(null), this.primaryReadTimeoutMs);
            }),
        ]);
        if (local) {
            if (local.type === 'data')
                return local.result;
            return this.finishRead(local, await loadRemote());
        }
        const remote = loadRemote();
        const first = await Promise.race([
            primary.then((outcome) => ({ route: 'primary', outcome })),
            remote.then((outcome) => ({ route: 'remote', outcome })),
        ]);
        if (first.outcome.type === 'data')
            return first.outcome.result;
        const other = first.route === 'primary' ? await remote : await primary;
        return this.finishRead(first.outcome, other);
    }
    loadFromSourcesShared(hash, options) {
        const pendingKey = this.pendingReadKey(hash, { ...options, skipPrimary: true });
        let pending = this.inflightReads.get(pendingKey);
        if (!pending) {
            pending = this.loadFromSources(hash, options)
                .catch((error) => ({ type: 'error', error: errorFrom(error) }))
                .finally(() => {
                if (this.inflightReads.get(pendingKey) === pending) {
                    this.inflightReads.delete(pendingKey);
                }
            });
            this.inflightReads.set(pendingKey, pending);
        }
        return pending;
    }
    finishRead(...outcomes) {
        const errors = [];
        for (const outcome of outcomes) {
            if (outcome.type === 'data')
                return outcome.result;
            if (outcome.type === 'error')
                errors.push(outcome.error);
        }
        if (errors.length > 0)
            throw combineErrors(errors);
        return null;
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
    readPrimary(hash) {
        const read = Promise.resolve()
            .then(async () => {
            const data = await this.primary.get(hash);
            if (data === null)
                return { type: 'no-result' };
            return {
                type: 'data',
                result: {
                    data: await this.verifyData(hash, data, this.primarySourceId),
                    sourceId: this.primarySourceId,
                },
            };
        })
            .catch((error) => ({ type: 'error', error: errorFrom(error) }));
        if (!Number.isFinite(this.requestTimeoutMs) || this.requestTimeoutMs <= 0) {
            return read;
        }
        return new Promise((resolve) => {
            let settled = false;
            const timeoutId = setTimeout(() => {
                if (settled)
                    return;
                settled = true;
                resolve({
                    type: 'error',
                    error: new Error(`Blob route ${this.primarySourceId} timed out after ${this.requestTimeoutMs}ms`),
                });
            }, this.requestTimeoutMs);
            void read.then((outcome) => {
                if (settled)
                    return;
                settled = true;
                clearTimeout(timeoutId);
                resolve(outcome);
            });
        });
    }
    pendingReadKey(hash, options) {
        const sourceKey = options.sourceIds && options.sourceIds.length > 0
            ? [...options.sourceIds].sort().join(',')
            : '*';
        return `${toHex(hash)}:${options.htl ?? BLOB_DEFAULT_HTL}:${options.skipPrimary === true ? 'skip-primary' : 'with-primary'}:${sourceKey}`;
    }
    async verifyData(expectedHash, data, sourceId) {
        const stableData = data.slice();
        const actualHash = await sha256(stableData);
        if (toHex(actualHash) !== toHex(expectedHash)) {
            throw new Error(`Blob route ${sourceId} returned corrupt data with a mismatched SHA-256 hash`);
        }
        return stableData;
    }
    async getCandidateSources(sourceIds) {
        const requested = sourceIds && sourceIds.length > 0
            ? new Set(sourceIds)
            : null;
        const combined = new Map();
        for (const provider of this.sourceProviders) {
            for (const source of await provider()) {
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
    async orderedSources(sourceIds) {
        const now = Date.now();
        const candidates = await this.getCandidateSources(sourceIds);
        return candidates.sort((left, right) => {
            const leftStats = this.statsBySource.get(left.id) ?? defaultStats();
            const rightStats = this.statsBySource.get(right.id) ?? defaultStats();
            const scoreDiff = scoreSource(rightStats, now) - scoreSource(leftStats, now);
            if (scoreDiff !== 0)
                return scoreDiff;
            return left.id.localeCompare(right.id);
        });
    }
    createInFlightSourceRequest(source, request) {
        const startedAt = Date.now();
        this.recordRequest(source.id);
        const task = {
            source,
            settled: false,
            promise: Promise.resolve({ type: 'no-result' }),
        };
        task.promise = new Promise((resolve) => {
            let completed = false;
            let timeoutId = null;
            const controller = new AbortController();
            const finish = (outcome) => {
                if (completed) {
                    return;
                }
                completed = true;
                if (timeoutId !== null) {
                    clearTimeout(timeoutId);
                }
                resolve(outcome);
            };
            if (Number.isFinite(this.requestTimeoutMs) && this.requestTimeoutMs > 0) {
                timeoutId = setTimeout(() => {
                    if (completed) {
                        return;
                    }
                    this.recordTimeout(source.id);
                    controller.abort();
                    finish({
                        type: 'error',
                        error: new Error(`Blob route ${source.id} timed out after ${this.requestTimeoutMs}ms`),
                    });
                }, this.requestTimeoutMs);
            }
            void Promise.resolve()
                .then(() => source.read(request, controller.signal))
                .then(async (reply) => {
                if (completed) {
                    return;
                }
                const elapsedMs = Math.max(1, Date.now() - startedAt);
                if (reply.type === 'data') {
                    const stableData = await this.verifyData(request.hash, reply.data, source.id);
                    if (completed)
                        return;
                    this.recordSuccess(source.id, elapsedMs);
                    void Promise.resolve()
                        .then(() => this.primary.put(request.hash, stableData))
                        .catch(() => false);
                    finish({
                        type: 'data',
                        result: { sourceId: source.id, data: stableData },
                    });
                    return;
                }
                this.recordMiss(source.id);
                finish({ type: 'no-result' });
            })
                .catch((error) => {
                if (completed) {
                    return;
                }
                this.recordFailure(source.id);
                finish({ type: 'error', error: errorFrom(error) });
            });
        });
        return task;
    }
    async waitForNextResult(inFlight, waitMs) {
        const active = inFlight.filter((task) => !task.settled);
        if (active.length === 0)
            return null;
        if (waitMs !== undefined && waitMs <= 0)
            return null;
        const outcomes = active.map((task) => task.promise.then((outcome) => ({ task, outcome })));
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
        const orderedSources = await this.orderedSources(options.sourceIds);
        if (orderedSources.length === 0) {
            return { type: 'no-result' };
        }
        const request = createBlobRequest(hash, options.htl);
        const dispatch = normalizeDispatchConfig(this.dispatch, orderedSources.length);
        const wavePlan = buildHedgedWavePlan(orderedSources.length, dispatch);
        if (wavePlan.length === 0)
            return { type: 'no-result' };
        const inFlight = [];
        const errors = [];
        let nextSourceIdx = 0;
        const accept = (outcome) => {
            if (outcome.type === 'data')
                return outcome.result;
            if (outcome.type === 'error')
                errors.push(outcome.error);
            return null;
        };
        for (let waveIdx = 0; waveIdx < wavePlan.length; waveIdx++) {
            const waveSize = wavePlan[waveIdx];
            const from = nextSourceIdx;
            const to = Math.min(from + waveSize, orderedSources.length);
            const waveSources = orderedSources.slice(from, to);
            nextSourceIdx = to;
            for (const source of waveSources) {
                inFlight.push(this.createInFlightSourceRequest(source, request));
            }
            const isLastWave = waveIdx === wavePlan.length - 1 || nextSourceIdx >= orderedSources.length;
            const windowEnd = isLastWave ? null : Date.now() + dispatch.hedgeIntervalMs;
            while (isLastWave || Date.now() < (windowEnd ?? 0)) {
                const remaining = windowEnd === null ? undefined : windowEnd - Date.now();
                const result = await this.waitForNextResult(inFlight, remaining);
                if (!result)
                    break;
                const data = accept(result.outcome);
                if (data)
                    return { type: 'data', result: data };
            }
        }
        return errors.length > 0
            ? { type: 'error', error: combineErrors(errors) }
            : { type: 'no-result' };
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