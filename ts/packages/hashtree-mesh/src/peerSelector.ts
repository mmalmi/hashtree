import {
  PEER_METADATA_SNAPSHOT_VERSION,
  type PeerMetadataSnapshot,
  type PersistedPeerMetadata,
  type RequestDispatchConfig,
  type SelectionStrategy,
} from './types.js';

const FAIRNESS_SELECTION_SHARE_WARNING = 0.30;
const FAIRNESS_MIN_PEERS = 5;
const INITIAL_BACKOFF_MS = 1000;
const BACKOFF_MULTIPLIER = 2;
const MAX_BACKOFF_MS = 480_000;
const MIN_RTO_MS = 50;
const MAX_RTO_MS = 60_000;
const INITIAL_RTO_MS = 1000;

interface PeerStats {
  peerId: string;
  connectedAtMs: number;
  requestsSent: number;
  successes: number;
  timeouts: number;
  failures: number;
  srttMs: number;
  rttvarMs: number;
  rtoMs: number;
  consecutiveRtoBackoffs: number;
  backoffLevel: number;
  backedOffUntilMs?: number;
  lastSuccessMs?: number;
  lastFailureMs?: number;
  bytesReceived: number;
  bytesSent: number;
}

function clamp01(n: number): number {
  return Math.max(0, Math.min(1, n));
}

function clampRtoMs(rtoMs: number): number {
  if (!Number.isFinite(rtoMs) || rtoMs <= 0) return INITIAL_RTO_MS;
  return Math.max(MIN_RTO_MS, Math.min(MAX_RTO_MS, Math.floor(rtoMs)));
}

function sanitizeLatency(value: number): number {
  if (!Number.isFinite(value) || value < 0) return 0;
  return value;
}

function defaultStats(peerId: string): PeerStats {
  return {
    peerId,
    connectedAtMs: Date.now(),
    requestsSent: 0,
    successes: 0,
    timeouts: 0,
    failures: 0,
    srttMs: 0,
    rttvarMs: 0,
    rtoMs: INITIAL_RTO_MS,
    consecutiveRtoBackoffs: 0,
    backoffLevel: 0,
    bytesReceived: 0,
    bytesSent: 0,
  };
}

function successRate(stats: PeerStats): number {
  if (stats.requestsSent === 0) return 0.5;
  return stats.successes / stats.requestsSent;
}

function isBackedOff(stats: PeerStats): boolean {
  return Boolean(stats.backedOffUntilMs && Date.now() < stats.backedOffUntilMs);
}

function scoreWeighted(stats: PeerStats): number {
  const reliability = successRate(stats);
  const latencyScore = stats.srttMs > 0 ? Math.min(1, 500 / (stats.srttMs + 50)) : 0.5;
  const recencyBonus =
    stats.lastSuccessMs && Date.now() - stats.lastSuccessMs < 60_000 ? 0.1 : 0;
  return 0.6 * reliability + 0.3 * latencyScore + 0.1 * (1 + recencyBonus);
}

function scoreUtilityUcb(stats: PeerStats, totalRequests: number): number {
  const good = stats.successes + 1;
  const bad = stats.failures + stats.timeouts + 1;
  const ratio = good / bad;
  const ratioScore = ratio / (1 + ratio);
  const latencyScore = stats.srttMs > 0 ? Math.min(1, 300 / (stats.srttMs + 50)) : 0.5;
  const efficiencyScore =
    stats.bytesSent > 0 ? Math.min(1, stats.bytesReceived / stats.bytesSent) : 0.5;
  const exploitation = 0.55 * ratioScore + 0.25 * latencyScore + 0.2 * efficiencyScore;
  const uncertainty = Math.sqrt(Math.log(totalRequests + 1) / (stats.requestsSent + 1));
  return exploitation + 0.2 * uncertainty;
}

function peerMetadataFromStats(principal: string, stats: PeerStats): PersistedPeerMetadata {
  return {
    principal,
    requestsSent: stats.requestsSent,
    successes: stats.successes,
    timeouts: stats.timeouts,
    failures: stats.failures,
    srttMs: sanitizeLatency(stats.srttMs),
    rttvarMs: sanitizeLatency(stats.rttvarMs),
    rtoMs: clampRtoMs(stats.rtoMs),
    bytesReceived: stats.bytesReceived,
    bytesSent: stats.bytesSent,
  };
}

function applyMetadata(stats: PeerStats, metadata: PersistedPeerMetadata): void {
  stats.requestsSent = Math.max(0, Math.floor(metadata.requestsSent));
  stats.successes = Math.max(0, Math.floor(metadata.successes));
  stats.timeouts = Math.max(0, Math.floor(metadata.timeouts));
  stats.failures = Math.max(0, Math.floor(metadata.failures));
  stats.srttMs = sanitizeLatency(metadata.srttMs);
  stats.rttvarMs = sanitizeLatency(metadata.rttvarMs);
  stats.rtoMs = clampRtoMs(metadata.rtoMs);
  stats.bytesReceived = Math.max(0, Math.floor(metadata.bytesReceived));
  stats.bytesSent = Math.max(0, Math.floor(metadata.bytesSent));
  stats.backoffLevel = 0;
  stats.backedOffUntilMs = undefined;
  stats.lastSuccessMs = undefined;
  stats.lastFailureMs = undefined;
  stats.consecutiveRtoBackoffs = 0;
}

function totalRequests(stats: PeerStats[]): number {
  let total = 0;
  for (const s of stats) total += s.requestsSent;
  return total;
}

export function peerPrincipal(peerId: string): string {
  return peerId;
}

export class PeerSelector {
  private stats = new Map<string, PeerStats>();
  private persistedMetadata = new Map<string, PersistedPeerMetadata>();
  private strategy: SelectionStrategy;
  private fairnessEnabled = true;
  private roundRobinIdx = 0;

  constructor(strategy: SelectionStrategy = 'weighted') {
    this.strategy = strategy;
  }

  static withStrategy(strategy: SelectionStrategy): PeerSelector {
    return new PeerSelector(strategy);
  }

  setFairness(enabled: boolean): void {
    this.fairnessEnabled = enabled;
  }

  setStrategy(strategy: SelectionStrategy): void {
    this.strategy = strategy;
  }

  getPeerIds(): string[] {
    return Array.from(this.stats.keys());
  }

  addPeer(peerId: string): void {
    if (this.stats.has(peerId)) return;
    const stats = defaultStats(peerId);
    const persisted = this.persistedMetadata.get(peerPrincipal(peerId));
    if (persisted) applyMetadata(stats, persisted);
    this.stats.set(peerId, stats);
  }

  removePeer(peerId: string): void {
    const stats = this.stats.get(peerId);
    if (!stats) return;
    const principal = peerPrincipal(peerId);
    this.persistedMetadata.set(principal, peerMetadataFromStats(principal, stats));
    this.stats.delete(peerId);
  }

  isPeerBackedOff(peerId: string): boolean {
    const stats = this.stats.get(peerId);
    return stats ? isBackedOff(stats) : false;
  }

  recordRequest(peerId: string, bytes: number): void {
    const stats = this.stats.get(peerId);
    if (!stats) return;
    stats.requestsSent += 1;
    stats.bytesSent += Math.max(0, Math.floor(bytes));
  }

  recordSuccess(peerId: string, rttMs: number, bytes: number): void {
    const stats = this.stats.get(peerId);
    if (!stats) return;

    const now = Date.now();
    stats.successes += 1;
    stats.bytesReceived += Math.max(0, Math.floor(bytes));
    stats.lastSuccessMs = now;
    stats.consecutiveRtoBackoffs = 0;
    stats.backoffLevel = 0;
    stats.backedOffUntilMs = undefined;

    const rtt = Math.max(0, rttMs);
    if (stats.srttMs === 0) {
      stats.srttMs = rtt;
      stats.rttvarMs = rtt / 2;
    } else {
      stats.rttvarMs = 0.75 * stats.rttvarMs + 0.25 * Math.abs(stats.srttMs - rtt);
      stats.srttMs = 0.875 * stats.srttMs + 0.125 * rtt;
    }
    const rto = stats.srttMs + Math.max(20, 4 * stats.rttvarMs);
    stats.rtoMs = clampRtoMs(rto);
  }

  recordTimeout(peerId: string): void {
    const stats = this.stats.get(peerId);
    if (!stats) return;
    const now = Date.now();
    stats.timeouts += 1;
    stats.lastFailureMs = now;
    stats.backoffLevel += 1;
    const backoffMs = Math.min(
      MAX_BACKOFF_MS,
      INITIAL_BACKOFF_MS * BACKOFF_MULTIPLIER ** (stats.backoffLevel - 1),
    );
    stats.backedOffUntilMs = now + backoffMs;

    if (stats.consecutiveRtoBackoffs < 5) {
      stats.rtoMs = Math.min(MAX_RTO_MS, stats.rtoMs * 2);
      stats.consecutiveRtoBackoffs += 1;
    }
  }

  recordFailure(peerId: string): void {
    const stats = this.stats.get(peerId);
    if (!stats) return;
    const now = Date.now();
    stats.failures += 1;
    stats.lastFailureMs = now;
    stats.backoffLevel += 1;
    const backoffMs = Math.min(
      MAX_BACKOFF_MS,
      INITIAL_BACKOFF_MS * BACKOFF_MULTIPLIER ** (stats.backoffLevel - 1),
    );
    stats.backedOffUntilMs = now + backoffMs;
  }

  selectPeers(): string[] {
    const allStats = Array.from(this.stats.values());
    if (allStats.length === 0) return [];

    const ready = allStats.filter((s) => !isBackedOff(s));
    const backedOff = allStats.filter((s) => isBackedOff(s));

    const total = totalRequests(allStats);
    let orderedReady = this.order(ready, total);
    if (this.fairnessEnabled && orderedReady.length >= FAIRNESS_MIN_PEERS) {
      orderedReady = this.applyFairness(orderedReady);
    }

    const orderedBackedOff = this.order(backedOff, total);
    return orderedReady.concat(orderedBackedOff).map((s) => s.peerId);
  }

  exportPeerMetadataSnapshot(): PeerMetadataSnapshot {
    const snapshot = new Map<string, PersistedPeerMetadata>(this.persistedMetadata);
    for (const [peerId, stats] of this.stats.entries()) {
      const principal = peerPrincipal(peerId);
      snapshot.set(principal, peerMetadataFromStats(principal, stats));
    }
    const peers = Array.from(snapshot.values()).sort((a, b) => a.principal.localeCompare(b.principal));
    return {
      version: PEER_METADATA_SNAPSHOT_VERSION,
      peers,
    };
  }

  importPeerMetadataSnapshot(snapshot: PeerMetadataSnapshot): void {
    if (!snapshot || snapshot.version !== PEER_METADATA_SNAPSHOT_VERSION) return;
    this.persistedMetadata.clear();
    for (const peer of snapshot.peers ?? []) {
      if (!peer?.principal) continue;
      this.persistedMetadata.set(peer.principal, {
        principal: peer.principal,
        requestsSent: Math.max(0, Math.floor(peer.requestsSent)),
        successes: Math.max(0, Math.floor(peer.successes)),
        timeouts: Math.max(0, Math.floor(peer.timeouts)),
        failures: Math.max(0, Math.floor(peer.failures)),
        srttMs: sanitizeLatency(peer.srttMs),
        rttvarMs: sanitizeLatency(peer.rttvarMs),
        rtoMs: clampRtoMs(peer.rtoMs),
        bytesReceived: Math.max(0, Math.floor(peer.bytesReceived)),
        bytesSent: Math.max(0, Math.floor(peer.bytesSent)),
      });
    }

    for (const [peerId, stats] of this.stats.entries()) {
      const persisted = this.persistedMetadata.get(peerPrincipal(peerId));
      if (persisted) applyMetadata(stats, persisted);
    }
  }

  private order(peers: PeerStats[], allTotalRequests: number): PeerStats[] {
    if (peers.length <= 1) return peers.slice();

    if (this.strategy === 'roundRobin') {
      const sorted = peers.slice().sort((a, b) => a.peerId.localeCompare(b.peerId));
      const offset = this.roundRobinIdx % sorted.length;
      this.roundRobinIdx = (this.roundRobinIdx + 1) % sorted.length;
      return sorted.slice(offset).concat(sorted.slice(0, offset));
    }

    if (this.strategy === 'random') {
      return peers.slice().sort(() => Math.random() - 0.5);
    }

    return peers.slice().sort((a, b) => this.scoreFor(b, allTotalRequests) - this.scoreFor(a, allTotalRequests));
  }

  private scoreFor(stats: PeerStats, allTotalRequests: number): number {
    switch (this.strategy) {
      case 'lowestLatency':
        return stats.srttMs > 0 ? -stats.srttMs : -1_000_000;
      case 'highestSuccessRate':
        return successRate(stats);
      case 'utilityUcb':
        return scoreUtilityUcb(stats, allTotalRequests);
      case 'weighted':
      default:
        return scoreWeighted(stats);
    }
  }

  private applyFairness(peers: PeerStats[]): PeerStats[] {
    const ordered = peers.slice();
    const total = totalRequests(ordered);
    if (total <= 0) return ordered;

    for (let i = 0; i < ordered.length; i += 1) {
      const top = ordered[0];
      const share = top.requestsSent / total;
      if (share <= FAIRNESS_SELECTION_SHARE_WARNING) break;
      ordered.push(ordered.shift()!);
    }
    return ordered;
  }
}

export function normalizeDispatchConfig(
  dispatch: RequestDispatchConfig,
  availablePeers: number,
): RequestDispatchConfig {
  let cap = dispatch.maxFanout === 0 ? availablePeers : Math.min(dispatch.maxFanout, availablePeers);
  if (cap < 0) cap = 0;

  const initialFanout = dispatch.initialFanout === 0 ? 1 : dispatch.initialFanout;
  const hedgeFanout = dispatch.hedgeFanout === 0 ? 1 : dispatch.hedgeFanout;
  const floor = Math.max(1, cap);

  return {
    initialFanout: Math.min(initialFanout, floor),
    hedgeFanout: Math.min(hedgeFanout, floor),
    maxFanout: cap,
    hedgeIntervalMs: Math.max(0, Math.floor(dispatch.hedgeIntervalMs)),
  };
}

export function buildHedgedWavePlan(
  peerCount: number,
  dispatch: RequestDispatchConfig,
): number[] {
  if (peerCount <= 0) return [];
  const cap = Math.min(dispatch.maxFanout, peerCount);
  if (cap <= 0) return [];

  const plan: number[] = [];
  let sent = 0;
  const first = Math.max(1, Math.min(dispatch.initialFanout, cap));
  plan.push(first);
  sent += first;

  while (sent < cap) {
    const next = Math.max(1, Math.min(dispatch.hedgeFanout, cap - sent));
    plan.push(next);
    sent += next;
  }

  return plan;
}

export function syncSelectorPeers(selector: PeerSelector, currentPeerIds: string[]): void {
  const current = new Set(currentPeerIds);
  for (const peerId of selector.getPeerIds()) {
    if (!current.has(peerId)) selector.removePeer(peerId);
  }
  for (const peerId of currentPeerIds) selector.addPeer(peerId);
}

export function blendWeightedScore(baseScore: number, boostWeight: number, boostValue: number): number {
  const weight = clamp01(boostWeight);
  const boost = clamp01(boostValue);
  return (1 - weight) * baseScore + weight * (baseScore + boost);
}
