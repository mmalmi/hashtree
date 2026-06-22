import {
  decryptKeyFromLink,
  fromHex,
  nhashEncode,
  toHex,
  type CID,
} from '@hashtree/core';
import type { StoredNostrEvent } from './events.js';
import type { Nip19Like, NostrFilter } from './resolver/nostr.js';
import {
  HASHTREE_ROOT_KINDS,
  parseHashtreeRootEvent,
  readSignedNostrEventSnapshot,
  storeSignedNostrEventSnapshot,
  type ParsedHashtreeRootEvent,
  type SnapshotTarget,
} from './snapshot.js';

const DEFAULT_SNAPSHOT_FETCH_LIMIT = 20;

export interface TreeEventSnapshotInfo extends ParsedHashtreeRootEvent {
  snapshotCid: CID;
  snapshotNhash: string;
  npub: string;
}

export interface TreeEventSnapshotQuery extends NostrFilter {
  limit?: number;
}

export interface FetchLatestTreeEventSnapshotConfig {
  snapshotTarget: SnapshotTarget;
  nip19: Nip19Like;
  fetchEvents: (filter: TreeEventSnapshotQuery) => Promise<StoredNostrEvent[]>;
  snapshotFetchLimit?: number;
}

export interface WatchTreeEventSnapshotsConfig extends FetchLatestTreeEventSnapshotConfig {
  subscribeEvents: (
    filter: TreeEventSnapshotQuery,
    onEvent: (event: StoredNostrEvent) => void,
  ) => () => void;
}

function compareEvents(a: StoredNostrEvent, b: StoredNostrEvent): number {
  if (a.created_at !== b.created_at) {
    return a.created_at - b.created_at;
  }
  return a.id.localeCompare(b.id);
}

function snapshotInfoFromParsed(
  parsed: ParsedHashtreeRootEvent,
  snapshotCid: CID,
  nip19: Nip19Like,
): TreeEventSnapshotInfo {
  return {
    ...parsed,
    snapshotCid,
    snapshotNhash: nhashEncode(snapshotCid),
    npub: nip19.npubEncode(parsed.event.pubkey),
  };
}

function tryParseHashtreeRootEvent(event: StoredNostrEvent): ParsedHashtreeRootEvent | null {
  try {
    return parseHashtreeRootEvent(event);
  } catch {
    return null;
  }
}

function normalizeLinkKey(linkKey?: string | Uint8Array | null): Uint8Array | null {
  if (!linkKey) {
    return null;
  }
  return typeof linkKey === 'string' ? fromHex(linkKey) : linkKey;
}

function isValidTreeEventFor(
  event: StoredNostrEvent,
  pubkey: string,
  treeName: string,
): boolean {
  if (event.pubkey !== pubkey) {
    return false;
  }
  const parsed = tryParseHashtreeRootEvent(event);
  return parsed?.treeName === treeName;
}

export function compareTreeEventSnapshots(a: TreeEventSnapshotInfo, b: TreeEventSnapshotInfo): number {
  return compareEvents(a.event, b.event);
}

export function isNewerTreeEventSnapshot(
  candidate: TreeEventSnapshotInfo,
  current: TreeEventSnapshotInfo,
): boolean {
  return compareTreeEventSnapshots(candidate, current) > 0;
}

export function snapshotMatchesRootCid(
  snapshot: TreeEventSnapshotInfo | null | undefined,
  rootCid: CID | null | undefined,
): boolean {
  if (!snapshot || !rootCid) {
    return false;
  }
  if (toHex(snapshot.rootCid.hash) !== toHex(rootCid.hash)) {
    return false;
  }
  if (snapshot.visibility !== 'public') {
    return true;
  }
  const snapshotKey = snapshot.rootCid.key ? toHex(snapshot.rootCid.key) : null;
  const rootKey = rootCid.key ? toHex(rootCid.key) : null;
  if (snapshotKey === null || rootKey === null) {
    return true;
  }
  return snapshotKey === rootKey;
}

export async function resolveSnapshotRootCid(
  snapshot: TreeEventSnapshotInfo,
  linkKey?: string | Uint8Array | null,
): Promise<CID | null> {
  if (snapshot.visibility === 'public') {
    return snapshot.rootCid;
  }

  if (snapshot.visibility === 'link-visible' && snapshot.encryptedKey) {
    const normalizedLinkKey = normalizeLinkKey(linkKey);
    if (!normalizedLinkKey) {
      return null;
    }
    try {
      const decryptedKey = await decryptKeyFromLink(
        fromHex(snapshot.encryptedKey),
        normalizedLinkKey,
      );
      if (decryptedKey) {
        return { hash: snapshot.rootCid.hash, key: decryptedKey };
      }
    } catch {
      return null;
    }
  }

  return null;
}

export async function storeTreeEventSnapshot(
  snapshotTarget: SnapshotTarget,
  nip19: Nip19Like,
  event: StoredNostrEvent,
): Promise<TreeEventSnapshotInfo | null> {
  const parsed = tryParseHashtreeRootEvent(event);
  if (!parsed) {
    return null;
  }
  const snapshotCid = await storeSignedNostrEventSnapshot(snapshotTarget, parsed.event);
  return snapshotInfoFromParsed(parsed, snapshotCid, nip19);
}

export async function readTreeEventSnapshot(
  snapshotTarget: SnapshotTarget,
  nip19: Nip19Like,
  snapshotCid: CID,
  maxBytes?: number,
): Promise<TreeEventSnapshotInfo | null> {
  try {
    const event = await readSignedNostrEventSnapshot(snapshotTarget, snapshotCid, maxBytes);
    const parsed = tryParseHashtreeRootEvent(event);
    if (!parsed) {
      return null;
    }
    return snapshotInfoFromParsed(parsed, snapshotCid, nip19);
  } catch {
    return null;
  }
}

export async function fetchLatestTreeEventSnapshot(
  config: FetchLatestTreeEventSnapshotConfig,
  npub: string,
  treeName: string,
): Promise<TreeEventSnapshotInfo | null> {
  let decoded;
  try {
    decoded = config.nip19.decode(npub);
  } catch {
    return null;
  }
  if (decoded.type !== 'npub' || typeof decoded.data !== 'string') {
    return null;
  }

  const events = await config.fetchEvents({
    kinds: [...HASHTREE_ROOT_KINDS],
    authors: [decoded.data],
    '#d': [treeName],
    limit: config.snapshotFetchLimit ?? DEFAULT_SNAPSHOT_FETCH_LIMIT,
  });

  const candidates: StoredNostrEvent[] = [];
  for (const event of events) {
    if (!isValidTreeEventFor(event, decoded.data, treeName)) {
      continue;
    }
    candidates.push(event);
  }
  if (candidates.length === 0) {
    return null;
  }

  candidates.sort(compareEvents);
  return storeTreeEventSnapshot(config.snapshotTarget, config.nip19, candidates[candidates.length - 1]!);
}

export function watchLatestTreeEventSnapshot(
  config: WatchTreeEventSnapshotsConfig,
  npub: string,
  treeName: string,
  onSnapshot: (snapshot: TreeEventSnapshotInfo) => void | Promise<void>,
): () => void {
  let decoded;
  try {
    decoded = config.nip19.decode(npub);
  } catch {
    return () => {};
  }
  if (decoded.type !== 'npub' || typeof decoded.data !== 'string') {
    return () => {};
  }
  const pubkey = decoded.data;

  let closed = false;
  let latestSnapshot: TreeEventSnapshotInfo | null = null;

  const emitIfNewer = async (event: StoredNostrEvent): Promise<void> => {
    if (closed || !isValidTreeEventFor(event, pubkey, treeName)) {
      return;
    }

    const snapshot = await storeTreeEventSnapshot(config.snapshotTarget, config.nip19, event);
    if (!snapshot) {
      return;
    }
    if (latestSnapshot && compareTreeEventSnapshots(snapshot, latestSnapshot) <= 0) {
      return;
    }

    latestSnapshot = snapshot;
    await onSnapshot(snapshot);
  };

  fetchLatestTreeEventSnapshot(config, npub, treeName)
    .then(async (snapshot) => {
      if (closed || !snapshot) {
        return;
      }
      if (latestSnapshot && compareTreeEventSnapshots(snapshot, latestSnapshot) <= 0) {
        return;
      }
      latestSnapshot = snapshot;
      await onSnapshot(snapshot);
    })
    .catch(() => {});

  const unsubscribe = config.subscribeEvents(
    {
      kinds: [...HASHTREE_ROOT_KINDS],
      authors: [pubkey],
      '#d': [treeName],
    },
    (event) => {
      void emitIfNewer(event);
    },
  );

  return () => {
    closed = true;
    unsubscribe();
  };
}
