import { HASHTREE_ROOT_KINDS, parseHashtreeRootEvent } from '@hashtree/nostr';
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
function withUniqueRelays(relays) {
    const seen = new Set();
    const result = [];
    const configuredRelays = (relays ?? []).map((relay) => relay.trim()).filter(Boolean);
    const relayCandidates = configuredRelays.length > 0
        ? configuredRelays
        : DEFAULT_ROOT_RESOLVE_RELAYS;
    for (const relay of relayCandidates) {
        const normalized = relay.trim();
        if (!normalized || seen.has(normalized))
            continue;
        seen.add(normalized);
        result.push(normalized);
    }
    return result;
}
function safeDecodePathSegment(segment) {
    try {
        return decodeURIComponent(segment);
    }
    catch {
        return segment;
    }
}
function splitPathSegments(path) {
    return path
        ?.split('/')
        .filter(Boolean)
        .map(safeDecodePathSegment) ?? [];
}
function compareReplaceableEventOrder(candidateCreatedAt, candidateEventId, currentCreatedAt, currentEventId) {
    const createdAtDiff = candidateCreatedAt - currentCreatedAt;
    if (createdAtDiff !== 0) {
        return createdAtDiff;
    }
    return candidateEventId.localeCompare(currentEventId);
}
function decodeNpub(npub) {
    try {
        const decoded = nip19.decode(npub);
        if (decoded.type !== 'npub' || typeof decoded.data !== 'string') {
            return null;
        }
        return decoded.data;
    }
    catch {
        return null;
    }
}
function parseRootLookupPath(path) {
    const pathSegments = splitPathSegments(path);
    const treeName = pathSegments[0] || 'public';
    const subPath = pathSegments.slice(1);
    return {
        treeName,
        subPath,
    };
}
function cidKey(cid) {
    if (!cid)
        return '';
    const keyHex = cid.key ? Array.from(cid.key).map((byte) => byte.toString(16).padStart(2, '0')).join('') : '';
    const hashHex = Array.from(cid.hash).map((byte) => byte.toString(16).padStart(2, '0')).join('');
    return keyHex ? `${hashHex}:${keyHex}` : hashHex;
}
function updateLatestRecord(current, createdAt, eventId, cid) {
    if (current && compareReplaceableEventOrder(createdAt, eventId, current.createdAt, current.eventId) <= 0) {
        return null;
    }
    return { createdAt, eventId, cid };
}
function preferLatestRecord(current, candidate) {
    if (!candidate) {
        return current;
    }
    return updateLatestRecord(current, candidate.createdAt, candidate.eventId, candidate.cid) ?? current ?? candidate;
}
function createSubscriptionId() {
    if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
        return crypto.randomUUID();
    }
    return `htree-root-${Math.random().toString(16).slice(2)}`;
}
function parseRelayMessage(data) {
    if (typeof data !== 'string') {
        return null;
    }
    try {
        const parsed = JSON.parse(data);
        return Array.isArray(parsed) ? parsed : null;
    }
    catch {
        return null;
    }
}
function openRelaySubscriptions(relays, filter, handlers) {
    const subId = createSubscriptionId();
    const sockets = [];
    let closed = false;
    for (const relay of relays) {
        let socket = null;
        try {
            socket = new WebSocket(relay);
        }
        catch {
            handlers.onError?.(relay);
            continue;
        }
        sockets.push(socket);
        socket.onopen = () => {
            if (closed) {
                try {
                    socket?.close();
                }
                catch {
                    // Ignore close errors.
                }
                return;
            }
            try {
                socket.send(JSON.stringify(['REQ', subId, filter]));
            }
            catch {
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
                handlers.onEvent?.(message[2], relay);
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
                }
                catch {
                    // Ignore close frame errors.
                }
                try {
                    socket.close();
                }
                catch {
                    // Ignore socket close errors.
                }
            }
        },
    };
}
async function resolvePreferredCid(tree, rootRecord, subPath) {
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
function cachedRootToRecord(cached) {
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
async function cacheParsedRootEvent(npub, event) {
    const parsed = parseHashtreeRootEvent(event);
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
async function queryLatestTreeRoot(relays, npub, treeName, timeoutMs, settleMs) {
    const pubkey = decodeNpub(npub);
    if (!pubkey) {
        return null;
    }
    return await new Promise((resolve) => {
        let closed = false;
        let latestRecord = null;
        let settleTimer = null;
        let timeoutId = null;
        const subscription = openRelaySubscriptions(relays, {
            kinds: [...HASHTREE_ROOT_KINDS],
            authors: [pubkey],
            '#d': [treeName],
            limit: MAX_TREE_ROOT_EVENTS,
        }, {
            onEvent(event) {
                const parsed = parseHashtreeRootEvent(event);
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
        const finish = (record) => {
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
export async function watchRootPathFromRelays(tree, relays, npub, path, onUpdate, timeoutMs = DEFAULT_ROOT_RESOLVE_TIMEOUT_MS, settleMs = DEFAULT_ROOT_RESOLVE_SETTLE_MS) {
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
    let treeRecord = cachedRootToRecord(await getCachedRootInfo(npub, treeName));
    if (!treeRecord) {
        const initialRecord = await queryLatestTreeRoot(relayList, npub, treeName, timeoutMs, settleMs);
        if (initialRecord) {
            treeRecord = initialRecord;
        }
    }
    let subscription = null;
    let settleTimer = null;
    let timeoutId = null;
    let resolveTicket = 0;
    let currentCidKey = null;
    const initialCid = await resolvePreferredCid(tree, treeRecord, subPath);
    let initialResolved = initialCid !== null;
    let closed = false;
    const close = async () => {
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
    const emitCurrent = async (mode) => {
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
    const settleInitial = () => {
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
    }
    else {
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
            const parsed = parseHashtreeRootEvent(event);
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
export async function resolveRootPathFromRelays(tree, relays, npub, path, timeoutMs = DEFAULT_ROOT_RESOLVE_TIMEOUT_MS, settleMs = DEFAULT_ROOT_RESOLVE_SETTLE_MS) {
    const relayList = withUniqueRelays(relays);
    const { treeName, subPath } = parseRootLookupPath(path);
    const cachedTreeRoot = cachedRootToRecord(await getCachedRootInfo(npub, treeName));
    if (cachedTreeRoot) {
        const shouldRefreshCachedRoot = !!cachedTreeRoot.cid.key;
        if (!shouldRefreshCachedRoot) {
            try {
                return await resolvePreferredCid(tree, cachedTreeRoot, subPath);
            }
            catch {
                // Fall through to a fresh relay lookup when the cached root no longer decodes locally.
            }
        }
        const refreshedTreeRoot = preferLatestRecord(cachedTreeRoot, await queryLatestTreeRoot(relayList, npub, treeName, timeoutMs, settleMs));
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
//# sourceMappingURL=rootResolver.js.map