import type { StoredNostrEvent } from './events.js';

const MAX_U64 = (1n << 64n) - 1n;

export const MANIFEST_BY_ID = 'by-id';
export const MANIFEST_BY_AUTHOR_TIME = 'by-author-time';
export const MANIFEST_BY_AUTHOR_KIND_TIME = 'by-author-kind-time';
export const MANIFEST_BY_KIND_TIME = 'by-kind-time';
export const MANIFEST_BY_KIND_TIME_AUTHOR = 'by-kind-time-author';
export const MANIFEST_BY_TIME = 'by-time';
export const MANIFEST_BY_TAG = 'by-tag';
export const MANIFEST_REPLACEABLE = 'replaceable';
export const MANIFEST_PARAMETERIZED_REPLACEABLE = 'parameterized-replaceable';

export function padKind(kind: number): string {
  return kind.toString(16).padStart(8, '0');
}

export function reverseTimestamp(createdAt: number): string {
  return (MAX_U64 - BigInt(createdAt)).toString(16).padStart(16, '0');
}

export function isReplaceableKind(kind: number): boolean {
  return kind === 0 || kind === 3 || (kind >= 10_000 && kind < 20_000);
}

export function isParameterizedReplaceableKind(kind: number): boolean {
  return kind >= 30_000 && kind < 40_000;
}

export function getDTag(event: Pick<StoredNostrEvent, 'tags'>): string | null {
  for (const tag of event.tags) {
    if (tag[0] === 'd' && typeof tag[1] === 'string' && tag[1].length > 0) {
      return tag[1];
    }
  }

  return null;
}

export function compareEvents(a: StoredNostrEvent, b: StoredNostrEvent): number {
  if (a.created_at !== b.created_at) {
    return a.created_at - b.created_at;
  }

  return a.id.localeCompare(b.id);
}

export function compareReplaceableEvents(a: StoredNostrEvent, b: StoredNostrEvent): number {
  if (a.created_at !== b.created_at) {
    return a.created_at - b.created_at;
  }

  return b.id.localeCompare(a.id);
}

export function authorTimeKey(event: StoredNostrEvent): string {
  return `${event.pubkey}:${reverseTimestamp(event.created_at)}:${event.id}`;
}

export function authorKindTimeKey(event: StoredNostrEvent): string {
  return `${event.pubkey}:${padKind(event.kind)}:${reverseTimestamp(event.created_at)}:${event.id}`;
}

export function kindTimeKey(event: StoredNostrEvent): string {
  return `${padKind(event.kind)}:${reverseTimestamp(event.created_at)}:${event.id}`;
}

export function kindTimeAuthorKey(event: StoredNostrEvent): string {
  return `${padKind(event.kind)}:${reverseTimestamp(event.created_at)}:${event.pubkey}:${event.id}`;
}

export function timeKey(event: StoredNostrEvent): string {
  return `${reverseTimestamp(event.created_at)}:${event.id}`;
}

export function createdAtFromIndexKey(key: string): number {
  const parts = key.split(':');
  if (parts.length < 2) {
    throw new Error(`Invalid Nostr index key: ${key}`);
  }

  const reversed = parts[parts.length - 2];
  const reversedTimestamp = BigInt(`0x${reversed}`);
  const createdAt = MAX_U64 - reversedTimestamp;
  if (createdAt > BigInt(Number.MAX_SAFE_INTEGER)) {
    throw new Error(`Created_at exceeds safe integer range in Nostr index key: ${key}`);
  }

  return Number(createdAt);
}

export function normalizeTagName(tagName: string): string {
  if (tagName.length === 0) {
    throw new Error('tag name must be non-empty');
  }

  return tagName.toLowerCase();
}

export function normalizeTagValue(tagName: string, tagValue: string): string {
  return tagName === 't' ? tagValue.toLowerCase() : tagValue;
}

export function tagKeys(event: StoredNostrEvent): string[] {
  return event.tags.flatMap((tag) => {
    const [name, value] = tag;
    if (!name || !value) {
      return [];
    }

    const normalizedName = name.toLowerCase();
    const normalizedValue = normalizeTagValue(normalizedName, value);
    return [`${normalizedName}:${normalizedValue}:${reverseTimestamp(event.created_at)}:${event.id}`];
  });
}

export function tagPrefix(tagName: string, tagValue: string): string {
  const normalizedName = normalizeTagName(tagName);
  return `${normalizedName}:${normalizeTagValue(normalizedName, tagValue)}:`;
}

export function replaceableKey(pubkey: string, kind: number): string {
  return `${pubkey}:${padKind(kind)}`;
}

export function parameterizedReplaceableKey(pubkey: string, kind: number, dTag: string): string {
  return `${pubkey}:${padKind(kind)}:${dTag}`;
}

export function retainLatestReplaceableEvents(events: StoredNostrEvent[]): StoredNostrEvent[] {
  const retained = new Map<string, StoredNostrEvent>();
  const passthrough: StoredNostrEvent[] = [];

  for (const event of events) {
    let slot: string | null = null;
    if (isReplaceableKind(event.kind)) {
      slot = `replaceable:${replaceableKey(event.pubkey, event.kind)}`;
    } else if (isParameterizedReplaceableKind(event.kind)) {
      slot = `parameterized:${parameterizedReplaceableKey(event.pubkey, event.kind, getDTag(event) ?? '')}`;
    }

    if (!slot) {
      passthrough.push(event);
      continue;
    }

    const existing = retained.get(slot);
    if (!existing || compareReplaceableEvents(event, existing) > 0) {
      retained.set(slot, event);
    }
  }

  return [...passthrough, ...retained.values()];
}
