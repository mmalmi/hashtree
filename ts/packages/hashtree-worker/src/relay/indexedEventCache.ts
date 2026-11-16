import type { Collection, Table } from 'dexie';
import {
  selectStoredEvents, verifyNostrEvent,
  type NostrEvent, type NostrFilter, type QueryOptions,
} from 'nostr-pubsub';

export interface EventRow {
  event: NostrEvent;
  order: number;
  tagTimes: [string, number][];
}

// Negative timestamps keep newest events first and equal timestamps in ID order.
export const EVENT_SCHEMA =
  '&event.id, order, [event.kind+order], [event.pubkey+order], [event.pubkey+event.kind+order], *tagTimes';

export function eventRow(event: NostrEvent): EventRow {
  const order = -event.created_at;
  const keys = new Set(event.tags.filter(tag => tag.length >= 2).map(tag => JSON.stringify(tag.slice(0, 2))));
  return { event, order, tagTimes: Array.from(keys, key => [key, order]) };
}

function checkTime(options: QueryOptions): void {
  options.signal?.throwIfAborted();
  if (options.deadline !== undefined && Date.now() >= options.deadline) throw new DOMException('Event query timed out', 'TimeoutError');
}

async function candidates(events: Table<EventRow, string>, filter: NostrFilter, options: QueryOptions): Promise<EventRow[]> {
  const cap = Math.min(options.limit ?? Infinity, filter.limit ?? Infinity);
  if (cap === 0 || (filter.since !== undefined && filter.until !== undefined && filter.since > filter.until)) return [];
  if (filter.ids) return (await events.bulkGet(filter.ids)).filter((row): row is EventRow => !!row);
  const tag = Object.entries(filter).find(([key]) => key.startsWith('#'));
  const first = filter.until === undefined ? -Infinity : -filter.until;
  const last = filter.since === undefined ? Infinity : -filter.since;
  const read = (range: Collection<EventRow, string>) => {
    // Remaining predicates and expiration must be applied before the per-range cap.
    const matches = range.filter(({ event }) => {
      checkTime(options);
      return selectStoredEvents([event], [filter]).length !== 0;
    });
    return (Number.isFinite(cap) ? matches.limit(cap) : matches).toArray();
  };
  let ranges: Collection<EventRow, string>[];
  if (tag) {
    ranges = (tag[1] as string[]).map(value => {
      const prefix = JSON.stringify([tag[0].slice(1), value]);
      return events.where('tagTimes').between([prefix, first], [prefix, last], true, true);
    });
  } else if (filter.authors?.length === 1 && filter.kinds?.length === 1) {
    const prefix = [filter.authors[0], filter.kinds[0]];
    ranges = [events.where('[event.pubkey+event.kind+order]').between([...prefix, first], [...prefix, last], true, true)];
  } else if (filter.kinds && (!filter.authors || filter.kinds.length <= filter.authors.length)) {
    ranges = filter.kinds.map(kind => events.where('[event.kind+order]').between([kind, first], [kind, last], true, true));
  } else if (filter.authors) {
    ranges = filter.authors.map(author => events.where('[event.pubkey+order]').between([author, first], [author, last], true, true));
  } else {
    ranges = [events.where('order').between(first, last, true, true)];
  }
  return (await Promise.all(ranges.map(read))).flat();
}

export async function queryIndexedEvents(
  events: Table<EventRow, string>, filters: NostrFilter[], options: QueryOptions,
): Promise<NostrEvent[]> {
  checkTime(options);
  if (filters.some(filter => filter.search !== undefined)) throw new Error('The local event cache does not support NIP-50 search');
  const rows = await events.db.transaction('r', events, () => Promise.all(filters.map(filter => candidates(events, filter, options))));
  checkTime(options);
  const unique = new Map(rows.flat().map(({ event }) => [event.id, event]));
  const selected = selectStoredEvents(unique.values(), filters, options);
  return selected.map(event => verifyNostrEvent(event));
}
