import { HashTree, cid, fromHex, type CID, type Store, type TreeVisibility } from '@hashtree/core';
import type { StoredNostrEvent } from './events.js';

const HEX_64 = /^[0-9a-f]{64}$/;
const HEX_128 = /^[0-9a-f]{128}$/;
const MAX_SNAPSHOT_BYTES = 256 * 1024;
const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder();

export const HASHTREE_ROOT_KIND = 30064;
export const HASHTREE_LEGACY_ROOT_KIND = 30078;
export const HASHTREE_ROOT_KINDS = [HASHTREE_ROOT_KIND, HASHTREE_LEGACY_ROOT_KIND] as const;
export const HASHTREE_LABEL = 'hashtree';

export function isHashtreeRootKind(kind: number): boolean {
  return kind === HASHTREE_ROOT_KIND || kind === HASHTREE_LEGACY_ROOT_KIND;
}

export interface ParsedHashtreeRootEvent {
  event: StoredNostrEvent;
  treeName: string;
  rootCid: CID;
  visibility: TreeVisibility;
  labels: string[];
  encryptedKey?: string;
  keyId?: string;
  selfEncryptedKey?: string;
  selfEncryptedLinkKey?: string;
}

export interface SnapshotTreeLike {
  putFile(
    data: Uint8Array,
    options?: { unencrypted?: boolean },
  ): Promise<{ cid: CID; size: number }>;
  readFileRange(id: CID, start: number, end?: number): Promise<Uint8Array | null>;
}

export type SnapshotTarget = Store | SnapshotTreeLike;

function isSnapshotTreeLike(target: SnapshotTarget): target is SnapshotTreeLike {
  return (
    typeof (target as SnapshotTreeLike).putFile === 'function' &&
    typeof (target as SnapshotTreeLike).readFileRange === 'function'
  );
}

function getSnapshotTree(target: SnapshotTarget): SnapshotTreeLike {
  if (isSnapshotTreeLike(target)) {
    return target;
  }
  return new HashTree({ store: target });
}

function assertStringTags(tags: unknown): asserts tags is string[][] {
  if (!Array.isArray(tags)) {
    throw new Error('Nostr event tags must be an array');
  }
  for (const tag of tags) {
    if (!Array.isArray(tag) || tag.some((value) => typeof value !== 'string')) {
      throw new Error('Nostr event tags must be an array of string arrays');
    }
  }
}

function uniqueLabels(labels: string[]): string[] {
  const seen = new Set<string>();
  const result: string[] = [];
  for (const label of labels) {
    if (!label || seen.has(label)) continue;
    seen.add(label);
    result.push(label);
  }
  return result;
}

export function normalizeSignedNostrEvent(event: StoredNostrEvent): StoredNostrEvent {
  const { id, pubkey, created_at, kind, tags, content, sig } = event;
  if (!HEX_64.test(id)) {
    throw new Error('Nostr event id must be a lowercase 64-character hex string');
  }
  if (!HEX_64.test(pubkey)) {
    throw new Error('Nostr event pubkey must be a lowercase 64-character hex string');
  }
  if (!Number.isInteger(created_at) || created_at < 0) {
    throw new Error('Nostr event created_at must be a non-negative integer');
  }
  if (!Number.isInteger(kind) || kind < 0) {
    throw new Error('Nostr event kind must be a non-negative integer');
  }
  assertStringTags(tags);
  if (typeof content !== 'string') {
    throw new Error('Nostr event content must be a string');
  }
  if (!HEX_128.test(sig)) {
    throw new Error('Nostr event sig must be a lowercase 128-character hex string');
  }

  return {
    id,
    pubkey,
    created_at,
    kind,
    tags: tags.map((tag) => [...tag]),
    content,
    sig,
  };
}

export function encodeSignedNostrEventJson(event: StoredNostrEvent): Uint8Array {
  const normalized = normalizeSignedNostrEvent(event);
  return textEncoder.encode(JSON.stringify({
    id: normalized.id,
    pubkey: normalized.pubkey,
    created_at: normalized.created_at,
    kind: normalized.kind,
    tags: normalized.tags,
    content: normalized.content,
    sig: normalized.sig,
  }));
}

export function decodeSignedNostrEventJson(data: Uint8Array): StoredNostrEvent {
  const parsed = JSON.parse(textDecoder.decode(data)) as Record<string, unknown>;
  return normalizeSignedNostrEvent({
    id: typeof parsed.id === 'string' ? parsed.id : '',
    pubkey: typeof parsed.pubkey === 'string' ? parsed.pubkey : '',
    created_at: typeof parsed.created_at === 'number' ? parsed.created_at : NaN,
    kind: typeof parsed.kind === 'number' ? parsed.kind : NaN,
    tags: parsed.tags,
    content: typeof parsed.content === 'string' ? parsed.content : '',
    sig: typeof parsed.sig === 'string' ? parsed.sig : '',
  } as StoredNostrEvent);
}

export async function storeSignedNostrEventSnapshot(target: SnapshotTarget, event: StoredNostrEvent): Promise<CID> {
  const tree = getSnapshotTree(target);
  return (await tree.putFile(encodeSignedNostrEventJson(event), { unencrypted: true })).cid;
}

export async function readSignedNostrEventSnapshot(
  target: SnapshotTarget,
  snapshotCid: CID,
  maxBytes = MAX_SNAPSHOT_BYTES,
): Promise<StoredNostrEvent> {
  const tree = getSnapshotTree(target);
  const bytes = await tree.readFileRange(snapshotCid, 0, maxBytes + 1);
  if (!bytes) {
    throw new Error('Signed Nostr event snapshot is missing');
  }
  if (bytes.length > maxBytes) {
    throw new Error(`Signed Nostr event snapshot exceeds ${maxBytes} bytes`);
  }
  return decodeSignedNostrEventJson(bytes);
}

function hasLabel(event: Pick<StoredNostrEvent, 'tags'>, label: string): boolean {
  return event.tags.some((tag) => tag[0] === 'l' && tag[1] === label);
}

function hasAnyLabel(event: Pick<StoredNostrEvent, 'tags'>): boolean {
  return event.tags.some((tag) => tag[0] === 'l');
}

export function parseHashtreeRootEvent(event: StoredNostrEvent): ParsedHashtreeRootEvent | null {
  const normalized = normalizeSignedNostrEvent(event);
  if (!isHashtreeRootKind(normalized.kind)) {
    return null;
  }

  const treeName = normalized.tags.find((tag) => tag[0] === 'd')?.[1];
  const hashHex = normalized.tags.find((tag) => tag[0] === 'hash')?.[1];
  if (!treeName || !hashHex || !HEX_64.test(hashHex)) {
    return null;
  }

  if (hasAnyLabel(normalized) && !hasLabel(normalized, HASHTREE_LABEL)) {
    return null;
  }

  const labels = uniqueLabels(
    normalized.tags
      .filter((tag) => tag[0] === 'l' && typeof tag[1] === 'string' && tag[1].length > 0)
      .map((tag) => tag[1]!)
  );
  const keyHex = normalized.tags.find((tag) => tag[0] === 'key')?.[1];
  const encryptedKey = normalized.tags.find((tag) => tag[0] === 'encryptedKey')?.[1];
  const keyId = normalized.tags.find((tag) => tag[0] === 'keyId')?.[1];
  const selfEncryptedKey = normalized.tags.find((tag) => tag[0] === 'selfEncryptedKey')?.[1];
  const selfEncryptedLinkKey = normalized.tags.find((tag) => tag[0] === 'selfEncryptedLinkKey')?.[1];

  const visibility: TreeVisibility = encryptedKey
    ? 'link-visible'
    : selfEncryptedKey
      ? 'private'
      : 'public';

  try {
    return {
      event: normalized,
      treeName,
      rootCid: cid(fromHex(hashHex), visibility === 'public' && keyHex ? fromHex(keyHex) : undefined),
      visibility,
      labels,
      encryptedKey,
      keyId,
      selfEncryptedKey,
      selfEncryptedLinkKey,
    };
  } catch {
    return null;
  }
}
