import type { Event, VerifiedEvent } from 'nostr-tools/core';
import { matchFilters, type Filter } from 'nostr-tools/filter';
import { verifyEvent } from 'nostr-tools/pure';

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

export function verifyAndFreezeNostrEvent(event: Event): NostrVerifiedEvent {
  const candidate: Event = {
    id: event.id,
    pubkey: event.pubkey,
    created_at: event.created_at,
    kind: event.kind,
    tags: event.tags.map((tag) => [...tag]),
    content: event.content,
    sig: event.sig,
  };
  if (!verifyEvent(candidate)) {
    throw new Error('invalid Nostr event id or signature');
  }
  if (
    !Number.isSafeInteger(candidate.created_at)
    || candidate.created_at < 0
    || !Number.isSafeInteger(candidate.kind)
    || candidate.kind < 0
    || candidate.kind > 65_535
  ) {
    throw new Error('invalid Nostr event timestamp or kind');
  }
  for (const tag of candidate.tags) Object.freeze(tag);
  Object.freeze(candidate.tags);
  return Object.freeze(candidate);
}
