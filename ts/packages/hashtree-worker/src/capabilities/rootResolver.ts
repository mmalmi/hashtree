import type { CID, HashTree } from '@hashtree/core';
import { HASHTREE_ROOT_KINDS, parseHashtreeRootEvent, type NostrEvent } from '@hashtree/nostr';
import { nip19 } from 'nostr-tools';
import { getCachedRootInfo, setCachedRoot } from '../relay/treeRootCache.js';

export const DEFAULT_ROOT_RESOLVE_TIMEOUT_MS = 15_000;
export const DEFAULT_ROOT_RESOLVE_SETTLE_MS = 500;

const MAX_TREE_ROOT_EVENTS = 8;
const DEFAULT_ROOT_RESOLVE_RELAYS = [
  'wss://relay.damus.io',
  'wss://relay.primal.net',
  'wss://relay.nostr.band',
  'wss://relay.snort.social',
];

function withUniqueRelays(relays?: string[]): string[] {
  const seen = new Set<string>();
  const result: string[] = [];
  const configuredRelays = (relays ?? []).map((relay) => relay.trim()).filter(Boolean);
  const relayCandidates = configuredRelays.length > 0
    ? configuredRelays
    : DEFAULT_ROOT_RESOLVE_RELAYS;

  for (const relay of relayCandidates) {
    const normalized = relay.trim();
    if (!normalized || seen.has(normalized)) continue;
    seen.add(normalized);
    result.push(normalized);
  }

  return result;
}

function safeDecodePathSegment(segment: string): string {
  try {
    return decodeURIComponent(segment);
  } catch {
    return segment;
  }
}

function splitPathSegments(path?: string): string[] {
  return path
    ?.split('/')
    .filter(Boolean)
    .map(safeDecodePathSegment) ?? [];
}

function compareReplaceableEventOrder(
  candidateCreatedAt: number,
  candidateEventId: string,
  currentCreatedAt: number,
  currentEventId: string,
): number {
  const createdAtDiff = candidateCreatedAt - currentCreatedAt;
  if (createdAtDiff !== 0) {
    return createdAtDiff;
  }

  return candidateEventId.localeCompare(currentEventId);
}

type ParsedRootPath = {
  treeName: string;
  subPath: string[];
};

type RootRecord = {
  createdAt: number;
  eventId: string;
  cid: CID;
};

export interface RootWatchHandle {
  initialCid: CID | null;
  close(): Promise<void>;
}

function decodeNpub(npub: string): string | null {
  try {
    const decoded = nip19.decode(npub);
    if (decoded.type !== 'npub' || typeof decoded.data !== 'string') {
      return null;
    }
    return decoded.data;
  } catch {
    return null;
  }
}

function parseRootLookupPath(path?: string): ParsedRootPath {
  const pathSegments = splitPathSegments(path);
  const treeName = pathSegments[0] || 'public';
  const subPath = pathSegments.slice(1);

  return {
    treeName,
    subPath,
  };
}

function cidKey(cid: CID | null): string {
  if (!cid) return '';
  const keyHex = cid.key ? Array.from(cid.key).map((byte) => byte.toString(16).padStart(2, '0')).join('') : '';
  const hashHex = Array.from(cid.hash).map((byte) => byte.toString(16).padStart(2, '0')).join('');
  return keyHex ? `${hashHex}:${keyHex}` : hashHex;
}

function updateLatestRecord(
  current: RootRecord | null,
  createdAt: number,
  eventId: string,
  cid: CID,
): RootRecord | null {
  if (current && compareReplaceableEventOrder(createdAt, eventId, current.createdAt, current.eventId) <= 0) {
    return null;
  }

  return { createdAt, eventId, cid };
}

function preferLatestRecord(current: RootRecord | null, candidate: RootRecord | null): RootRecord | null {
  if (!candidate) {
    return current;
  }

  return updateLatestRecord(current, candidate.createdAt, candidate.eventId, candidate.cid) ?? current ?? candidate;
}

function createSubscriptionId(): string {
  if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
    return crypto.randomUUID();
  }
  return `htree-root-${Math.random().toString(16).slice(2)}`;
}

function parseRelayMessage(data: unknown): unknown[] | null {
  if (typeof data !== 'string') {
    return null;
  }

  try {
    const parsed = JSON.parse(data);
    return Array.isArray(parsed) ? parsed : null;
  } catch {
    return null;
  }
}

function openRelaySubscriptions(
  relays: string[],
  filter: Record<string, unknown>,
  handlers: {
    onEvent?: (event: NostrEvent, relay: string) => void;
    onEose?: (relay: string) => void;
    onError?: (relay: string) => void;
  },
): { close(): Promise<void> } {
  const subId = createSubscriptionId();
  const sockets: WebSocket[] = [];
  let closed = false;

  for (const relay of relays) {
    let socket: WebSocket | null = null;
    try {
      socket = new WebSocket(relay);
    } catch {
      handlers.onError?.(relay);
      continue;
    }

    sockets.push(socket);

    socket.onopen = () => {
      if (closed) {
        try {
          socket?.close();
        } catch {
          // Ignore close errors.
        }
        return;
      }

      try {
        socket.send(JSON.stringify(['REQ', subId, filter]));
      } catch {
        handlers.onError?.(relay);
      }
    };

    socket.onerror = () => {
      handlers.onError?.(relay);
    };

    socket.onmessage = (event) => {
      const message = parseRelayMessage(event.data);
      if (!message || message[1] !== subId) {
        return;
      }

      if (message[0] === 'EVENT' && message[2] && typeof message[2] === 'object') {
        handlers.onEvent?.(message[2] as NostrEvent, relay);
        return;
      }

      if (message[0] === 'EOSE' || message[0] === 'CLOSED') {
        handlers.onEose?.(relay);
      }
    };
  }

  return {
    async close() {
      if (closed) {
        return;
      }
      closed = true;

      for (const socket of sockets) {
        try {
          socket.send(JSON.stringify(['CLOSE', subId]));
        } catch {
          // Ignore close frame errors.
        }
        try {
          socket.close();
        } catch {
          // Ignore socket close errors.
        }
      }
    },
  };
}

async function resolvePreferredCid(
  tree: Pick<HashTree, 'resolvePath'> | null,
  rootRecord: RootRecord | null,
  subPath: string[],
): Promise<CID | null> {
  if (!rootRecord) {
    return null;
  }

  if (subPath.length === 0) {
    return rootRecord.cid;
  }

  if (!tree) {
    throw new Error('Tree not initialized');
  }

  return (await tree.resolvePath(rootRecord.cid, subPath))?.cid ?? null;
}

function cachedRootToRecord(
  cached: Awaited<ReturnType<typeof getCachedRootInfo>>,
): RootRecord | null {
  if (!cached) {
    return null;
  }

  return {
    createdAt: cached.updatedAt ?? 0,
    eventId: cached.eventId ?? '',
    cid: {
      hash: cached.hash,
      key: cached.key,
    },
  };
}

async function cacheParsedRootEvent(npub: string, event: NostrEvent): Promise<void> {
  const parsed = parseHashtreeRootEvent(event as Parameters<typeof parseHashtreeRootEvent>[0]);
  if (!parsed) {
    return;
  }

  await setCachedRoot(npub, parsed.treeName, parsed.rootCid, parsed.visibility, {
    updatedAt: event.created_at ?? 0,
    eventId: event.id,
    labels: parsed.labels,
    encryptedKey: parsed.encryptedKey,
    keyId: parsed.keyId,
    selfEncryptedKey: parsed.selfEncryptedKey,
    selfEncryptedLinkKey: parsed.selfEncryptedLinkKey,
  });
}

async function queryLatestTreeRoot(
  relays: string[],
  npub: string,
  treeName: string,
  timeoutMs: number,
  settleMs: number,
): Promise<RootRecord | null> {
  const pubkey = decodeNpub(npub);
  if (!pubkey) {
    return null;
  }

  return await new Promise<RootRecord | null>((resolve) => {
    let closed = false;
    let latestRecord: RootRecord | null = null;
    let settleTimer: ReturnType<typeof setTimeout> | null = null;
    let timeoutId: ReturnType<typeof setTimeout> | null = null;

    const subscription = openRelaySubscriptions(relays, {
      kinds: [...HASHTREE_ROOT_KINDS],
      authors: [pubkey],
      '#d': [treeName],
      limit: MAX_TREE_ROOT_EVENTS,
    }, {
      onEvent(event) {
        const parsed = parseHashtreeRootEvent(event as Parameters<typeof parseHashtreeRootEvent>[0]);
        if (!parsed || parsed.treeName !== treeName) {
          return;
        }

        const nextRecord = updateLatestRecord(latestRecord, event.created_at ?? 0, event.id ?? '', parsed.rootCid);
        if (!nextRecord) {
          return;
        }

        latestRecord = nextRecord;
        void cacheParsedRootEvent(npub, event);
        if (settleTimer) {
          clearTimeout(settleTimer);
        }
        settleTimer = setTimeout(() => {
          finish(latestRecord);
        }, settleMs);
      },
      onError() {
        // Ignore individual relay failures and let timeout decide.
      },
      onEose() {
        // Slower relays may still provide a newer replaceable event.
      },
    });

    const finish = (record: RootRecord | null): void => {
      if (closed) {
        return;
      }
      closed = true;
      if (settleTimer) {
        clearTimeout(settleTimer);
      }
      if (timeoutId) {
        clearTimeout(timeoutId);
      }
      void subscription.close().finally(() => {
        resolve(record);
      });
    };

    timeoutId = setTimeout(() => {
      finish(latestRecord);
    }, timeoutMs);
  });
}

export async function watchRootPathFromRelays(
  tree: Pick<HashTree, 'resolvePath'> | null,
  relays: string[] | undefined,
  npub: string,
  path: string | undefined,
  onUpdate: (cid: CID | null) => void | Promise<void>,
  timeoutMs: number = DEFAULT_ROOT_RESOLVE_TIMEOUT_MS,
  settleMs: number = DEFAULT_ROOT_RESOLVE_SETTLE_MS,
): Promise<RootWatchHandle> {
  const relayList = withUniqueRelays(relays);
  const pubkey = decodeNpub(npub);
  if (!pubkey) {
    return {
      initialCid: null,
      async close() {
        // no-op
      },
    };
  }

  const { treeName, subPath } = parseRootLookupPath(path);
  let treeRecord: RootRecord | null = cachedRootToRecord(await getCachedRootInfo(npub, treeName));
  if (!treeRecord) {
    const initialRecord = await queryLatestTreeRoot(relayList, npub, treeName, timeoutMs, settleMs);
    if (initialRecord) {
      treeRecord = initialRecord;
    }
  }
  let subscription: { close(): Promise<void> } | null = null;
  let settleTimer: ReturnType<typeof setTimeout> | null = null;
  let timeoutId: ReturnType<typeof setTimeout> | null = null;
  let resolveTicket = 0;
  let currentCidKey: string | null = null;
  const initialCid = await resolvePreferredCid(tree, treeRecord, subPath);
  let initialResolved = initialCid !== null;
  let closed = false;

  const close = async (): Promise<void> => {
    if (closed) {
      return;
    }
    closed = true;

    if (settleTimer) {
      clearTimeout(settleTimer);
    }
    if (timeoutId) {
      clearTimeout(timeoutId);
    }

    await Promise.resolve(subscription?.close()).catch(() => undefined);
  };

  const emitCurrent = async (mode: 'initial' | 'update'): Promise<CID | null> => {
    const ticket = ++resolveTicket;
    const cid = await resolvePreferredCid(tree, treeRecord, subPath);
    if (closed || ticket !== resolveTicket) {
      return cid;
    }

    const nextKey = cidKey(cid);
    if (mode === 'initial') {
      if (settleTimer) {
        clearTimeout(settleTimer);
        settleTimer = null;
      }
      if (timeoutId) {
        clearTimeout(timeoutId);
        timeoutId = null;
      }
      initialResolved = true;
      currentCidKey = nextKey;
      return cid;
    }

    if (currentCidKey === nextKey) {
      return cid;
    }

    currentCidKey = nextKey;
    await onUpdate(cid);
    return cid;
  };

  const settleInitial = (): void => {
    if (settleTimer) {
      clearTimeout(settleTimer);
    }
    settleTimer = setTimeout(() => {
      void emitCurrent('initial').then((cid) => {
        if (!closed) {
          void onUpdate(cid);
        }
      });
    }, settleMs);
  };

  if (initialCid) {
    currentCidKey = cidKey(initialCid);
  } else {
    timeoutId = setTimeout(() => {
      void emitCurrent('initial').then((cid) => {
        if (!closed) {
          void onUpdate(cid);
        }
      });
    }, timeoutMs);
  }

  subscription = openRelaySubscriptions(relayList, {
    kinds: [...HASHTREE_ROOT_KINDS],
    authors: [pubkey],
    '#d': [treeName],
    limit: MAX_TREE_ROOT_EVENTS,
  }, {
    onEvent(event) {
      const parsed = parseHashtreeRootEvent(event as Parameters<typeof parseHashtreeRootEvent>[0]);
      if (!parsed || parsed.treeName !== treeName) {
        return;
      }

      const nextRecord = updateLatestRecord(treeRecord, event.created_at ?? 0, event.id ?? '', parsed.rootCid);
      if (!nextRecord) {
        return;
      }
      treeRecord = nextRecord;

      void cacheParsedRootEvent(npub, event);

      if (!initialResolved) {
        settleInitial();
        return;
      }

      void emitCurrent('update');
    },
    onEose() {
      // Ignore faster relay EOSE notifications. The live watch keeps listening.
    },
    onError() {
      // Ignore relay close notifications. Other relays may still be active.
    },
  });

  return {
    initialCid,
    close,
  };
}

export async function resolveRootPathFromRelays(
  tree: Pick<HashTree, 'resolvePath'> | null,
  relays: string[] | undefined,
  npub: string,
  path?: string,
  timeoutMs: number = DEFAULT_ROOT_RESOLVE_TIMEOUT_MS,
  settleMs: number = DEFAULT_ROOT_RESOLVE_SETTLE_MS,
): Promise<CID | null> {
  const relayList = withUniqueRelays(relays);
  const { treeName, subPath } = parseRootLookupPath(path);
  const cachedTreeRoot = cachedRootToRecord(await getCachedRootInfo(npub, treeName));
  if (cachedTreeRoot) {
    const shouldRefreshCachedRoot = !!cachedTreeRoot.cid.key;
    if (!shouldRefreshCachedRoot) {
      try {
        return await resolvePreferredCid(tree, cachedTreeRoot, subPath);
      } catch {
        // Fall through to a fresh relay lookup when the cached root no longer decodes locally.
      }
    }

    const refreshedTreeRoot = preferLatestRecord(
      cachedTreeRoot,
      await queryLatestTreeRoot(relayList, npub, treeName, timeoutMs, settleMs),
    );
    if (refreshedTreeRoot) {
      return await resolvePreferredCid(tree, refreshedTreeRoot, subPath);
    }
    return null;
  }

  const root = await queryLatestTreeRoot(relayList, npub, treeName, timeoutMs, settleMs);
  if (!root) {
    return null;
  }

  return await resolvePreferredCid(tree, root, subPath);
}
