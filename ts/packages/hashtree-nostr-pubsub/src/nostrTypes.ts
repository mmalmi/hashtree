import {
  verifiedSymbol,
  type Event,
  type VerifiedEvent,
} from 'nostr-tools/pure';
import { matchFilters, type Filter } from 'nostr-tools/filter';

export type NostrEvent = Event;
export type NostrFilter = Filter;
export type NostrVerifiedEvent = VerifiedEvent;

export type NostrEventSourceKind =
  | 'local-index'
  | 'peer'
  | 'fips-endpoint'
  | 'relay';

export interface NostrEventSource {
  id: string;
  kind: NostrEventSourceKind;
  url?: string;
}

export interface NostrReaderQueryOptions {
  limit?: number;
  signal?: AbortSignal;
  /** Absolute Unix epoch deadline in milliseconds. */
  deadline?: number;
}

export interface NostrReaderQueryEvent {
  event: NostrVerifiedEvent;
  source: NostrEventSource;
  priority: number;
}

export interface NostrReaderQueryReport {
  events: NostrReaderQueryEvent[];
  complete?: boolean;
}

/** Structural equivalent of nostr-pubsub 0.4's reader interface. */
export interface NostrEventReaderContract {
  query(
    filters: NostrFilter[],
    options?: NostrReaderQueryOptions,
  ): Promise<NostrReaderQueryReport>;
}

export function cloneNostrFilter(filter: NostrFilter): NostrFilter {
  const cloned: NostrFilter = { ...filter };
  for (const [key, value] of Object.entries(filter)) {
    if (Array.isArray(value)) {
      (cloned as Record<string, unknown>)[key] = [...value];
    }
  }
  return cloned;
}

export function filtersMatchNostrEvents(
  filters: readonly NostrFilter[],
  event: Event,
): boolean {
  return filters.length === 0 || matchFilters([...filters], event);
}

export function cloneNostrEvent(event: Event): NostrEvent {
  if (event === null || typeof event !== 'object') {
    throw new Error('invalid Nostr event shape');
  }
  if (
    !isLowercaseHex(event.id, 64)
    || !isLowercaseHex(event.pubkey, 64)
    || !isLowercaseHex(event.sig, 128)
    || typeof event.content !== 'string'
    || !Array.isArray(event.tags)
  ) {
    throw new Error('invalid Nostr event shape');
  }
  if (
    !Number.isSafeInteger(event.created_at)
    || event.created_at < 0
    || !Number.isSafeInteger(event.kind)
    || event.kind < 0
    || event.kind > 65_535
  ) {
    throw new Error('invalid Nostr event timestamp or kind');
  }
  const tags = event.tags.map((tag) => {
    if (!Array.isArray(tag) || tag.some((value) => typeof value !== 'string')) {
      throw new Error('invalid Nostr event tag');
    }
    return [...tag];
  });
  return {
    id: event.id,
    pubkey: event.pubkey,
    created_at: event.created_at,
    kind: event.kind,
    tags,
    content: event.content,
    sig: event.sig,
  };
}

export function markNostrEventVerifiedAndFreeze(event: NostrEvent): NostrVerifiedEvent {
  event[verifiedSymbol] = true;
  for (const tag of event.tags) Object.freeze(tag);
  Object.freeze(event.tags);
  return Object.freeze(event) as NostrVerifiedEvent;
}

function isLowercaseHex(value: unknown, length: number): value is string {
  return typeof value === 'string'
    && value.length === length
    && /^[0-9a-f]+$/.test(value);
}
