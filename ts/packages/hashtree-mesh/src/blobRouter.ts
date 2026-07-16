import {
  BLOB_MAX_HTL,
  BLOB_NO_RESULT,
  blobData,
  createBlobRequest,
  verifyBlobData,
  type BlobReply,
  type BlobRequest,
  type BlobRoute,
  type BlobRouteContext,
  type Hash,
  type Store,
} from '@hashtree/core';

const OUTCOME_DECAY = 0.875;
const SEARCH_CANCELLED = Symbol('blob-router-cancelled');

export interface BlobRouterConfig {
  id?: string;
  cache?: Store;
  requestTimeoutMs?: number;
  maxRoutes?: number;
  maxRouteAttempts?: number;
  routeAttemptBudget?: number;
  maxInFlight?: number;
  hedgeDelayMs?: number;
  initialCooldownMs?: number;
  maxCooldownMs?: number;
  explorationInterval?: number;
}

interface NormalizedConfig {
  requestTimeoutMs: number;
  maxRoutes: number;
  maxRouteAttempts: number;
  routeAttemptBudget: number;
  maxInFlight: number;
  hedgeDelayMs: number;
  initialCooldownMs: number;
  maxCooldownMs: number;
  explorationInterval: number;
}

export interface BlobRouterReadOptions {
  htl?: number;
  preferredRouteIds?: readonly string[];
  allowedRouteIds?: readonly string[];
  context?: BlobRouteContext;
}

export interface BlobRouterResult {
  data: Uint8Array;
  routeId: string;
}

export interface BlobRouteOutcomeSnapshot {
  successfulWeight: number;
  failureWeight: number;
  timeoutWeight: number;
  successfulLatencyMs?: number;
  coolingDown: boolean;
}

interface RouteOutcome {
  successes: number;
  failures: number;
  timeouts: number;
  successfulLatencyMs?: number;
  consecutiveFailures: number;
  cooldownUntilMs?: number;
  lastAttemptMs?: number;
}

type AttemptOutcome =
  | { type: 'data'; route: BlobRoute; data: Uint8Array; elapsedMs: number }
  | { type: 'no-result'; route: BlobRoute }
  | { type: 'failure'; route: BlobRoute; error: Error };

interface Attempt {
  route: BlobRoute;
  controller: AbortController;
  promise: Promise<AttemptOutcome>;
}

const DEFAULT_CONFIG: NormalizedConfig = {
  requestTimeoutMs: 10_000,
  maxRoutes: 32,
  maxRouteAttempts: 32,
  routeAttemptBudget: 4,
  maxInFlight: 2,
  hedgeDelayMs: 75,
  initialCooldownMs: 250,
  maxCooldownMs: 10_000,
  explorationInterval: 16,
};

function asError(error: unknown): Error {
  return error instanceof Error ? error : new Error(String(error));
}

function finiteInteger(name: string, value: number, minimum: number): number {
  if (!Number.isSafeInteger(value) || value < minimum) {
    throw new RangeError(`${name} must be an integer of at least ${minimum}`);
  }
  return value;
}

function finiteDuration(name: string, value: number, minimum: number): number {
  if (!Number.isFinite(value) || value < minimum) {
    throw new RangeError(`${name} must be at least ${minimum}ms`);
  }
  return value;
}

function normalizeConfig(config: BlobRouterConfig): NormalizedConfig {
  return {
    requestTimeoutMs: finiteDuration(
      'requestTimeoutMs',
      config.requestTimeoutMs ?? DEFAULT_CONFIG.requestTimeoutMs,
      1,
    ),
    maxRoutes: finiteInteger('maxRoutes', config.maxRoutes ?? DEFAULT_CONFIG.maxRoutes, 1),
    maxRouteAttempts: finiteInteger(
      'maxRouteAttempts',
      config.maxRouteAttempts ?? DEFAULT_CONFIG.maxRouteAttempts,
      1,
    ),
    routeAttemptBudget: finiteInteger(
      'routeAttemptBudget',
      config.routeAttemptBudget ?? DEFAULT_CONFIG.routeAttemptBudget,
      1,
    ),
    maxInFlight: finiteInteger(
      'maxInFlight',
      config.maxInFlight ?? DEFAULT_CONFIG.maxInFlight,
      1,
    ),
    hedgeDelayMs: finiteDuration(
      'hedgeDelayMs',
      config.hedgeDelayMs ?? DEFAULT_CONFIG.hedgeDelayMs,
      0,
    ),
    initialCooldownMs: finiteDuration(
      'initialCooldownMs',
      config.initialCooldownMs ?? DEFAULT_CONFIG.initialCooldownMs,
      0,
    ),
    maxCooldownMs: finiteDuration(
      'maxCooldownMs',
      config.maxCooldownMs ?? DEFAULT_CONFIG.maxCooldownMs,
      0,
    ),
    explorationInterval: finiteInteger(
      'explorationInterval',
      config.explorationInterval ?? DEFAULT_CONFIG.explorationInterval,
      0,
    ),
  };
}

function validateRoutes(routes: readonly BlobRoute[], maxRoutes: number): void {
  if (routes.length > maxRoutes) {
    throw new Error(`Blob router has ${routes.length} routes, exceeding its bound of ${maxRoutes}`);
  }
  const ids = new Set<string>();
  for (const route of routes) {
    if (!route.id) throw new Error('Blob route identity must not be empty');
    if (ids.has(route.id)) throw new Error(`Duplicate blob route identity ${route.id}`);
    ids.add(route.id);
  }
}

function defaultOutcome(): RouteOutcome {
  return {
    successes: 0,
    failures: 0,
    timeouts: 0,
    consecutiveFailures: 0,
  };
}

function decay(outcome: RouteOutcome): void {
  outcome.successes *= OUTCOME_DECAY;
  outcome.failures *= OUTCOME_DECAY;
  outcome.timeouts *= OUTCOME_DECAY;
}

function score(outcome: RouteOutcome): number {
  const reliability = (outcome.successes + 1)
    / (outcome.successes + outcome.failures + outcome.timeouts + 2);
  const latency = outcome.successfulLatencyMs === undefined
    ? 0.5
    : 50 / (outcome.successfulLatencyMs + 50);
  return 0.7 * reliability + 0.3 * latency;
}

function sleep(ms: number): { promise: Promise<void>; cancel: () => void } {
  let timeout: ReturnType<typeof setTimeout> | undefined;
  return {
    promise: new Promise((resolve) => {
      timeout = setTimeout(resolve, ms);
    }),
    cancel: () => {
      if (timeout !== undefined) clearTimeout(timeout);
    },
  };
}

/** Adaptive read routing across opaque local or network blob targets. */
export class BlobRouter implements BlobRoute {
  readonly id: string;
  private routes: BlobRoute[];
  private readonly cache?: Store;
  private readonly config: NormalizedConfig;
  private readonly routeOutcomes = new Map<string, RouteOutcome>();
  private searches = 0;

  constructor(routes: readonly BlobRoute[], config: BlobRouterConfig = {}) {
    this.id = config.id ?? 'blob-router';
    if (!this.id) throw new Error('Blob router identity must not be empty');
    this.config = normalizeConfig(config);
    this.cache = config.cache;
    validateRoutes(routes, this.config.maxRoutes);
    this.routes = [...routes];
    for (const route of routes) this.routeOutcomes.set(route.id, defaultOutcome());
  }

  setRoutes(routes: readonly BlobRoute[]): void {
    validateRoutes(routes, this.config.maxRoutes);
    const previous = new Map(this.routes.map((route) => [route.id, route]));
    for (const [id] of this.routeOutcomes) {
      const replacement = routes.find((route) => route.id === id);
      if (!replacement || previous.get(id) !== replacement) this.routeOutcomes.delete(id);
    }
    for (const route of routes) {
      if (!this.routeOutcomes.has(route.id)) this.routeOutcomes.set(route.id, defaultOutcome());
    }
    this.routes = [...routes];
  }

  routeCount(): number {
    return this.routes.length;
  }

  outcomes(): Record<string, BlobRouteOutcomeSnapshot> {
    const now = Date.now();
    return Object.fromEntries([...this.routeOutcomes].map(([id, outcome]) => [id, {
      successfulWeight: outcome.successes,
      failureWeight: outcome.failures,
      timeoutWeight: outcome.timeouts,
      successfulLatencyMs: outcome.successfulLatencyMs,
      coolingDown: (outcome.cooldownUntilMs ?? 0) > now,
    }]));
  }

  async get(hash: Hash, preferredRouteIds?: readonly string[]): Promise<Uint8Array | null> {
    return (await this.getDetailed(hash, { preferredRouteIds }))?.data ?? null;
  }

  getDetailed(hash: Hash, options: BlobRouterReadOptions = {}): Promise<BlobRouterResult | null> {
    return this.getRequest(createBlobRequest(hash, options.htl), options);
  }

  async read(request: BlobRequest, context?: BlobRouteContext): Promise<BlobReply> {
    const result = await this.getRequest(request, { context });
    return result ? blobData(result.data) : BLOB_NO_RESULT;
  }

  async getRequest(
    request: BlobRequest,
    options: BlobRouterReadOptions = {},
  ): Promise<BlobRouterResult | null> {
    if (!Number.isSafeInteger(request.htl) || request.htl < 0 || request.htl > BLOB_MAX_HTL) {
      throw new RangeError(`Blob request HTL must be an integer from 0 to ${BLOB_MAX_HTL}`);
    }
    const outerSignal = options.context?.signal;
    if (outerSignal?.aborted) {
      throw new Error('Blob retrieval was cancelled before the search started');
    }
    const now = Date.now();
    const ownDeadline = now + this.config.requestTimeoutMs;
    const deadlineMs = Math.min(options.context?.deadlineMs ?? ownDeadline, ownDeadline);
    if (deadlineMs <= now) throw new Error('Blob retrieval deadline expired before the search started');

    const outerBudget = options.context?.attemptBudget ?? this.config.maxRouteAttempts;
    const maxAttempts = Math.min(this.config.maxRouteAttempts, outerBudget);
    if (maxAttempts < 1) throw new Error('Blob retrieval has no route attempt budget');

    const ordered = this.orderedRoutes(options.preferredRouteIds, options.allowedRouteIds);
    if (ordered.length === 0) return null;
    const attemptLimit = Math.min(maxAttempts, ordered.length);
    const maxInFlight = Math.min(this.config.maxInFlight, attemptLimit);
    const attempts = new Map<string, Attempt>();
    const failures: Error[] = [];
    let nextRoute = 0;
    let attempted = 0;
    let cancelSearch: (() => void) | undefined;
    const cancellation = outerSignal
      ? new Promise<typeof SEARCH_CANCELLED>((resolve) => {
        cancelSearch = () => resolve(SEARCH_CANCELLED);
        outerSignal.addEventListener('abort', cancelSearch, { once: true });
      })
      : undefined;
    const clearCancellation = (): void => {
      if (cancelSearch) outerSignal?.removeEventListener('abort', cancelSearch);
    };

    const launch = (): void => {
      const route = ordered[nextRoute++];
      attempted += 1;
      const controller = new AbortController();
      const abort = (): void => controller.abort(outerSignal?.reason);
      outerSignal?.addEventListener('abort', abort, { once: true });
      const routeContext: BlobRouteContext = {
        signal: controller.signal,
        deadlineMs,
        attemptBudget: Math.max(1, Math.min(this.config.routeAttemptBudget, outerBudget)),
      };
      const started = Date.now();
      const promise = Promise.resolve()
        .then(() => route.read(request, routeContext))
        .then((reply): AttemptOutcome => reply.type === 'data'
          ? { type: 'data', route, data: reply.data, elapsedMs: Math.max(1, Date.now() - started) }
          : { type: 'no-result', route })
        .catch((error: unknown): AttemptOutcome => ({ type: 'failure', route, error: asError(error) }))
        .finally(() => outerSignal?.removeEventListener('abort', abort));
      attempts.set(route.id, { route, controller, promise });
    };

    const abortPending = (): void => {
      for (const attempt of attempts.values()) attempt.controller.abort();
    };

    launch();
    let nextHedgeMs = Date.now() + this.config.hedgeDelayMs;

    while (attempts.size > 0) {
      const waitUntilMs = Math.min(nextHedgeMs, deadlineMs);
      const timer = sleep(Math.max(0, waitUntilMs - Date.now()));
      const racers: Array<Promise<AttemptOutcome | null | typeof SEARCH_CANCELLED>> = [
        ...[...attempts.values()].map((attempt) => attempt.promise),
        timer.promise.then(() => null),
      ];
      if (cancellation) racers.push(cancellation);
      const winner = await Promise.race(racers);
      timer.cancel();

      if (winner === SEARCH_CANCELLED) {
        abortPending();
        clearCancellation();
        throw new Error('Blob retrieval was cancelled');
      }
      if (winner === null) {
        if (Date.now() >= deadlineMs) {
          for (const attempt of attempts.values()) this.recordTimeout(attempt.route.id);
          abortPending();
          clearCancellation();
          throw new Error('Blob retrieval timed out before the search completed');
        }
        if (attempted < attemptLimit && attempts.size < maxInFlight) {
          launch();
          nextHedgeMs = Date.now() + this.config.hedgeDelayMs;
        }
        continue;
      }

      attempts.delete(winner.route.id);
      if (winner.type === 'data') {
        try {
          const data = await verifyBlobData(request.hash, winner.data, winner.route.id);
          this.recordSuccess(winner.route.id, winner.elapsedMs);
          abortPending();
          clearCancellation();
          if (this.cache) void this.cache.put(request.hash, data).catch(() => false);
          return { data, routeId: winner.route.id };
        } catch (error) {
          this.recordFailure(winner.route.id);
          failures.push(asError(error));
        }
      } else if (winner.type === 'no-result') {
        this.recordNoResult(winner.route.id);
      } else {
        this.recordFailure(winner.route.id);
        failures.push(new Error(`Blob route ${winner.route.id}: ${winner.error.message}`));
      }

      if (attempted < attemptLimit && attempts.size < maxInFlight) {
        launch();
        nextHedgeMs = Date.now() + this.config.hedgeDelayMs;
      }
    }

    if (attempted < ordered.length) {
      clearCancellation();
      throw new Error(
        `Blob retrieval exhausted its bounded route attempt budget (${attempted}/${ordered.length})`,
      );
    }
    if (failures.length > 0) {
      clearCancellation();
      throw new AggregateError(failures, `Blob retrieval was incomplete: ${failures[0].message}`);
    }
    clearCancellation();
    return null;
  }

  private orderedRoutes(
    preferredRouteIds?: readonly string[],
    allowedRouteIds?: readonly string[],
  ): BlobRoute[] {
    const allowed = allowedRouteIds ? new Set(allowedRouteIds) : null;
    const routes = (allowed
      ? this.routes.filter((route) => allowed.has(route.id) || (route.groupId && allowed.has(route.groupId)))
      : [...this.routes])
      .filter((route) => route.isAvailable?.() ?? true);
    const preferred = new Map((preferredRouteIds ?? []).map((id, index) => [id, index]));
    const now = Date.now();
    this.searches = (this.searches + 1) % Number.MAX_SAFE_INTEGER;
    const explore = this.config.explorationInterval > 0
      && this.searches % this.config.explorationInterval === 0;
    return routes.sort((left, right) => {
      const leftPreferred = preferred.get(left.id) ?? (left.groupId ? preferred.get(left.groupId) : undefined);
      const rightPreferred = preferred.get(right.id) ?? (right.groupId ? preferred.get(right.groupId) : undefined);
      if (leftPreferred !== undefined || rightPreferred !== undefined) {
        if (leftPreferred === undefined) return 1;
        if (rightPreferred === undefined) return -1;
        if (leftPreferred !== rightPreferred) return leftPreferred - rightPreferred;
      }
      const leftOutcome = this.outcome(left.id);
      const rightOutcome = this.outcome(right.id);
      if (explore) {
        const attemptDiff = (leftOutcome.lastAttemptMs ?? 0) - (rightOutcome.lastAttemptMs ?? 0);
        return attemptDiff || left.id.localeCompare(right.id);
      }
      const cooldownDiff = Number((leftOutcome.cooldownUntilMs ?? 0) > now)
        - Number((rightOutcome.cooldownUntilMs ?? 0) > now);
      const scoreDiff = score(rightOutcome) - score(leftOutcome);
      const attemptDiff = (leftOutcome.lastAttemptMs ?? 0) - (rightOutcome.lastAttemptMs ?? 0);
      return cooldownDiff || scoreDiff || attemptDiff || left.id.localeCompare(right.id);
    });
  }

  private outcome(routeId: string): RouteOutcome {
    let outcome = this.routeOutcomes.get(routeId);
    if (!outcome) {
      outcome = defaultOutcome();
      this.routeOutcomes.set(routeId, outcome);
    }
    return outcome;
  }

  private recordSuccess(routeId: string, elapsedMs: number): void {
    const outcome = this.outcome(routeId);
    decay(outcome);
    outcome.successes += 1;
    outcome.successfulLatencyMs = outcome.successfulLatencyMs === undefined
      ? elapsedMs
      : 0.875 * outcome.successfulLatencyMs + 0.125 * elapsedMs;
    outcome.consecutiveFailures = 0;
    outcome.cooldownUntilMs = undefined;
    outcome.lastAttemptMs = Date.now();
  }

  private recordNoResult(routeId: string): void {
    const outcome = this.outcome(routeId);
    decay(outcome);
    outcome.consecutiveFailures = 0;
    outcome.cooldownUntilMs = undefined;
    outcome.lastAttemptMs = Date.now();
  }

  private recordFailure(routeId: string): void {
    const outcome = this.outcome(routeId);
    decay(outcome);
    outcome.failures += 1;
    this.applyCooldown(outcome);
  }

  private recordTimeout(routeId: string): void {
    const outcome = this.outcome(routeId);
    decay(outcome);
    outcome.timeouts += 1;
    this.applyCooldown(outcome);
  }

  private applyCooldown(outcome: RouteOutcome): void {
    outcome.consecutiveFailures += 1;
    const multiplier = 2 ** Math.min(16, outcome.consecutiveFailures - 1);
    const cooldownMs = Math.min(
      this.config.maxCooldownMs,
      this.config.initialCooldownMs * multiplier,
    );
    outcome.lastAttemptMs = Date.now();
    outcome.cooldownUntilMs = outcome.lastAttemptMs + cooldownMs;
  }
}
