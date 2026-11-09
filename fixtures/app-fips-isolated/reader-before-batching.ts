import { CollectionSource } from '@hashtree/collection';
import type { CID } from '@hashtree/core';
import { raceAsyncIteratorWithPromise } from './asyncIteratorRace';
import {
  MANIFEST_BY_AUTHOR_KIND_TIME,
  MANIFEST_BY_AUTHOR_TIME,
  MANIFEST_BY_KIND_TIME,
  MANIFEST_BY_KIND_TIME_AUTHOR,
  MANIFEST_BY_TIME,
} from '@hashtree/nostr';
import { mergeRecentEvents } from './nostrEventMerge';
import {
  hasCollectionIndex,
  type NostrCollectionIndexes,
} from './nostrCollectionIndexes';
import {
  createConcurrencyLimiter,
  DEFAULT_INDEXED_EVENT_READ_CONCURRENCY,
  mapOrderedConcurrent,
  type ConcurrencyLimiter,
} from './orderedConcurrent';
import {
  authorFromKindTimeAuthorKey,
  createdAtFromKindTimeIndexKey,
  isStrictlyBeforeRecentCursor,
  streamTimeOrderedIndex,
  timeAndEventIdFromIndexKey,
  type RecentTextNoteQueryOptions,
} from './recentTextNoteCursor';
import type { RawEvent } from './nostr';
import { isNip10ReplyTag } from './nostrReplyTags';
import {
  MAX_RECENT_AUTHOR_FALLBACK_CHUNK,
  streamChunkedAuthorFeed,
} from './recentAuthorFeedChunks';
export { MAX_RECENT_AUTHOR_FALLBACK_CHUNK } from './recentAuthorFeedChunks';

const AUTHOR_TIME_POST_SCAN_LIMIT = 256;
const GLOBAL_RECENT_TEXT_NOTE_AUTHOR_THRESHOLD = 12;
const INDEX_COLLECTION_SOURCE_GRACE_MS = 600;
const MIN_RECENT_NOTE_SCAN_LIMIT = 128;
const MIN_SINGLE_AUTHOR_FAST_SCAN_LIMIT = 32;
const RECENT_NOTE_SCAN_BATCH = 16;
const RECENT_NOTE_SCAN_MULTIPLIER = 96;
const SINGLE_AUTHOR_FAST_SCAN_MULTIPLIER = 8;
const FAST_RECENT_NOTE_READ_CONCURRENCY = 4;
// Note one plus one normal Home batch can paint while a sparse kind cursor
// searches for its first result. The exact merge still owns the final window.
const FAST_RECENT_NOTE_PREVIEW_LIMIT = 9;

export const DURABLE_POST_KINDS = [1, 6, 20, 30_023] as const;

type RecentCursor = {
  iteratorIndex: number;
  event: RawEvent;
};

type FastRecentNoteProbe = {
  iterator: AsyncIterator<RawEvent>;
  cancel(): void;
};

export type RecentAuthorFeedReaderDeps = {
  getCollectionIndexes(options?: { timeoutMs?: number }): Promise<NostrCollectionIndexes>;
  getCollectionSource(): Promise<CollectionSource>;
  readEvent(cid: CID): Promise<RawEvent | null>;
  indexedEventReadConcurrency?: number;
  signal?: AbortSignal | null;
};

const padKind = (kind: number): string => kind.toString(16).padStart(8, '0');

function getRecentTextNoteKinds(options: RecentTextNoteQueryOptions): readonly number[] {
  return [...new Set(options.kinds ?? DURABLE_POST_KINDS)];
}

function isWithinTime(event: RawEvent, options: RecentTextNoteQueryOptions): boolean {
  return (options.since === undefined || event.created_at >= options.since)
    && (options.until === undefined || event.created_at <= options.until);
}

function isFilteredReply(event: RawEvent): boolean {
  // Kind-6 e-tags identify the repost target; only kind-1 e-tags are replies.
  return event.kind === 1 && event.tags.some(isNip10ReplyTag);
}

/**
 * The complete recent-author query policy shared by the browser reader and the
 * Hashtree worker. Storage and verification are injected so importing this
 * module never pulls the rest of the social data plane into the worker bundle.
 */
export class RecentAuthorFeedReader {
  private readonly concurrency: number;
  private readonly limiter: ConcurrencyLimiter;
  private readonly signal: AbortSignal | null;

  constructor(private readonly deps: RecentAuthorFeedReaderDeps) {
    this.concurrency = deps.indexedEventReadConcurrency
      ?? DEFAULT_INDEXED_EVENT_READ_CONCURRENCY;
    this.limiter = createConcurrencyLimiter(this.concurrency);
    this.signal = deps.signal ?? null;
  }

  async *stream(
    authors: string[],
    limit: number,
    options: RecentTextNoteQueryOptions = {},
  ): AsyncGenerator<RawEvent[], void, void> {
    const uniqueAuthors = [...new Set(authors)];
    if (uniqueAuthors.length === 0 || limit <= 0) {
      yield [];
      return;
    }

    const { byAuthorTime, byAuthorKindTime, byKindTime, byKindTimeAuthor } =
      await this.deps.getCollectionIndexes({ timeoutMs: INDEX_COLLECTION_SOURCE_GRACE_MS });
    if (this.aborted()) {
      return;
    }
    const preferGlobalRecent = uniqueAuthors.length >= GLOBAL_RECENT_TEXT_NOTE_AUTHOR_THRESHOLD;
    let globalRecentEvents: RawEvent[] = [];

    if (!preferGlobalRecent && byAuthorKindTime) {
      let yieldedAny = false;
      for await (const notes of this.streamByAuthorKindTime(uniqueAuthors, limit, options)) {
        if (this.aborted()) {return;}
        yieldedAny = true;
        yield notes;
      }
      if (yieldedAny) {return;}
    }

    if (byKindTimeAuthor) {
      let yieldedAny = false;
      for await (const notes of this.streamByKindTimeAuthor(
        new Set(uniqueAuthors), limit, options,
      )) {
        if (this.aborted()) {return;}
        yieldedAny = true;
        globalRecentEvents = notes;
        yield notes;
      }
      if (
        yieldedAny
        && (globalRecentEvents.length >= limit || !preferGlobalRecent || !byAuthorKindTime)
      ) {
        return;
      }
    }

    if (globalRecentEvents.length === 0 && byKindTime) {
      const notes = await this.getByKindTime(new Set(uniqueAuthors), limit, options);
      if (this.aborted()) {return;}
      if (notes.length > 0) {
        yield notes;
        if (!preferGlobalRecent || !byAuthorKindTime) {return;}
        globalRecentEvents = notes;
      }
    }

    if (byAuthorTime) {
      let yieldedAny = false;
      for await (const notes of this.streamByAuthorTime(uniqueAuthors, limit, options)) {
        if (this.aborted()) {return;}
        yieldedAny = true;
        yield globalRecentEvents.length > 0
          ? mergeRecentEvents(limit, globalRecentEvents, notes)
          : notes;
      }
      if (yieldedAny) {return;}
    }

    if (preferGlobalRecent && byAuthorKindTime) {
      let yieldedAny = false;
      for await (const notes of this.streamByAuthorKindTime(uniqueAuthors, limit, options)) {
        if (this.aborted()) {return;}
        yieldedAny = true;
        yield globalRecentEvents.length > 0
          ? mergeRecentEvents(limit, globalRecentEvents, notes)
          : notes;
      }
      if (yieldedAny) {return;}
    }

    if (!this.aborted()) {yield [];}
  }

  private aborted(): boolean {
    return this.signal?.aborted ?? false;
  }

  private async readEvent(cid: CID): Promise<RawEvent | null> {
    return this.aborted() ? null : await this.deps.readEvent(cid);
  }

  private async nextCursor(
    iteratorIndex: number,
    iterator: AsyncIterator<RawEvent>,
    options: RecentTextNoteQueryOptions,
  ): Promise<RecentCursor | null> {
    while (!this.aborted()) {
      const next = await iterator.next();
      if (next.done || this.aborted()) {return null;}
      const event = next.value;
      if (!isStrictlyBeforeRecentCursor(event.created_at, event.id, options.before)) {continue;}
      if (options.until !== undefined && event.created_at > options.until) {continue;}
      if (options.since !== undefined && event.created_at < options.since) {
        await iterator.return?.();
        return null;
      }
      if (!options.includeReplies && isFilteredReply(event)) {continue;}
      return { iteratorIndex, event };
    }
    return null;
  }

  private async *streamByAuthorKindTime(
    authors: string[],
    limit: number,
    options: RecentTextNoteQueryOptions,
  ): AsyncGenerator<RawEvent[], void, void> {
    if (authors.length > MAX_RECENT_AUTHOR_FALLBACK_CHUNK) {
      yield* streamChunkedAuthorFeed(
        authors,
        limit,
        () => this.aborted(),
        (chunk, chunkIndex) => this.streamByAuthorKindTimeChunk(
          chunk,
          limit,
          options,
          chunkIndex === 0,
        ),
      );
      return;
    }
    yield* this.streamByAuthorKindTimeChunk(authors, limit, options, true);
  }

  private async *streamByAuthorKindTimeChunk(
    authors: string[],
    limit: number,
    options: RecentTextNoteQueryOptions,
    useFastProbe: boolean,
  ): AsyncGenerator<RawEvent[], void, void> {
    const kinds = getRecentTextNoteKinds(options);
    const source = await this.deps.getCollectionSource();
    if (this.aborted()) {return;}
    const authorSet = new Set(authors);
    const fastProbe = useFastProbe
      ? this.createFastProbe(
          source, authorSet, limit, options, 'Failed to read fast recent post by author',
        )
      : undefined;
    const iterators = authors.flatMap((author) => kinds.map((kind) => (
      this.streamAuthorKindCandidates(source, author, kind, limit, options)[Symbol.asyncIterator]()
    )));
    yield* this.streamMerged(
      iterators,
      new Set(kinds),
      limit,
      options,
      'Failed to stream recent indexed posts by author',
      fastProbe,
    );
  }

  private async *streamAuthorKindCandidates(
    source: CollectionSource,
    author: string,
    kind: number,
    limit: number,
    options: RecentTextNoteQueryOptions,
  ): AsyncGenerator<RawEvent, void, void> {
    const candidates = (async function* (): AsyncGenerator<CID, void, void> {
      for await (const { key, cid } of streamTimeOrderedIndex(
        source,
        MANIFEST_BY_AUTHOR_KIND_TIME,
        `${author}:${padKind(kind)}:`,
        options.before,
      )) {
        const { createdAt, eventId } = timeAndEventIdFromIndexKey(key);
        if (options.until !== undefined && createdAt > options.until) {continue;}
        if (!isStrictlyBeforeRecentCursor(createdAt, eventId, options.before)) {continue;}
        if (options.since !== undefined && createdAt < options.since) {break;}
        yield cid;
      }
    })();
    for await (const event of mapOrderedConcurrent(
      candidates,
      async (cid) => await this.limiter.run(async () => await this.readEvent(cid)),
      { concurrency: Math.min(this.concurrency, Math.max(limit, 1)) },
    )) {
      if (this.aborted()) {return;}
      if (event && event.pubkey === author && event.kind === kind) {yield event;}
    }
  }

  private async *streamFastRecentTextNotes(
    source: CollectionSource,
    authorSet: Set<string>,
    limit: number,
    options: RecentTextNoteQueryOptions,
    isCancelled: () => boolean,
  ): AsyncGenerator<RawEvent, void, void> {
    if (options.before) {return;}
    const singleAuthor = authorSet.size === 1 ? [...authorSet][0] : null;
    const indexName = singleAuthor && hasCollectionIndex(source, MANIFEST_BY_AUTHOR_TIME)
      ? MANIFEST_BY_AUTHOR_TIME
      : hasCollectionIndex(source, MANIFEST_BY_TIME)
        ? MANIFEST_BY_TIME
        : null;
    if (!indexName) {return;}

    const kindSet = new Set(getRecentTextNoteKinds(options));
    const maxScanned = singleAuthor
      ? Math.max(MIN_SINGLE_AUTHOR_FAST_SCAN_LIMIT, limit * SINGLE_AUTHOR_FAST_SCAN_MULTIPLIER)
      : Math.max(MIN_RECENT_NOTE_SCAN_LIMIT, limit * RECENT_NOTE_SCAN_MULTIPLIER);
    // A small ordered batch overlaps cold reads without flooding the exact
    // kind queries or changing which verified post is presented first.
    const candidates = source.streamQueryIndex(indexName, {
      prefix: indexName === MANIFEST_BY_AUTHOR_TIME ? `${singleAuthor}:` : '',
      limit: maxScanned,
    });
    let presented = 0;
    const presentedIds = new Set<string>();
    const previewLimit = Math.min(limit, FAST_RECENT_NOTE_PREVIEW_LIMIT);
    for await (const event of mapOrderedConcurrent(
      candidates,
      ({ cid }) => this.limiter.run(async () => isCancelled() || this.aborted() ? null : await this.readEvent(cid)),
      { concurrency: Math.min(this.concurrency, FAST_RECENT_NOTE_READ_CONCURRENCY, Math.max(limit, 1)), waitForPendingOnClose: false },
    )) {
      if (isCancelled() || this.aborted()) {return;}
      if (event
        && !presentedIds.has(event.id)
        && authorSet.has(event.pubkey)
        && kindSet.has(event.kind)
        && isWithinTime(event, options)
        && (options.includeReplies || !isFilteredReply(event))
      ) {
        presentedIds.add(event.id);
        yield event;
        presented += 1;
        if (presented >= previewLimit) {return;}
      }
    }
  }

  private createFastProbe(
    source: CollectionSource,
    authorSet: Set<string>,
    limit: number,
    options: RecentTextNoteQueryOptions,
    warning: string,
  ): FastRecentNoteProbe {
    let cancelled = false;
    const reader = this;
    return {
      cancel: () => { cancelled = true; },
      iterator: (async function* (): AsyncGenerator<RawEvent, void, void> {
        try {
          yield* reader.streamFastRecentTextNotes(
            source, authorSet, limit, options, () => cancelled,
          );
        } catch (error) {
          console.warn(warning, error);
        }
      })(),
    };
  }

  private async *streamMerged(
    iterators: AsyncIterator<RawEvent>[],
    kindSet: Set<number>,
    limit: number,
    options: RecentTextNoteQueryOptions,
    warning: string,
    fastProbe?: FastRecentNoteProbe,
  ): AsyncGenerator<RawEvent[], void, void> {
    const emitEvery = Math.max(options.emitEvery ?? 1, 1);
    const ready: RecentCursor[] = [];
    const events: RawEvent[] = [];
    const seenIds = new Set<string>();
    let yieldedCount = 0;
    let lastYieldedIds = '';

    try {
      const initialCursorsPromise = Promise.all(iterators.map(async (iterator, iteratorIndex) => (
        await this.nextCursor(iteratorIndex, iterator, options).catch((error) => {
          console.warn(warning, error);
          return null;
        })
      )));
      let initialCursors: Array<RecentCursor | null> = [];
      if (fastProbe) {
        const preview: RawEvent[] = [];
        for await (const update of raceAsyncIteratorWithPromise(
          fastProbe.iterator, initialCursorsPromise,
        )) {
          if (this.aborted()) {return;}
          if (update.type === 'resolution') {
            initialCursors = update.value;
          } else {
            preview.push(update.value);
            if (preview.length === 1 || preview.length === limit
              || preview.length - yieldedCount >= emitEvery) {
              yieldedCount = preview.length;
              lastYieldedIds = preview.map(event => event.id).join(',');
              yield [...preview];
            }
          }
        }
        fastProbe.cancel();
      } else {
        initialCursors = await initialCursorsPromise;
      }
      if (this.aborted()) {return;}
      for (const cursor of initialCursors) {if (cursor) {ready.push(cursor);}}

      while (ready.length > 0 && events.length < limit && !this.aborted()) {
        ready.sort((left, right) => (
          right.event.created_at - left.event.created_at || left.event.id.localeCompare(right.event.id)
        ));
        const cursor = ready.shift()!;
        const event = cursor.event;
        if (!seenIds.has(event.id) && kindSet.has(event.kind)) {
          seenIds.add(event.id);
          events.push(event);
          events.sort((left, right) => (
            right.created_at - left.created_at || left.id.localeCompare(right.id)
          ));
          if (events.length >= yieldedCount && (
            events.length === 1 || events.length === limit || events.length - yieldedCount >= emitEvery
          )) {
            yieldedCount = events.length;
            const eventIds = events.map((candidate) => candidate.id).join(',');
            if (eventIds !== lastYieldedIds) {lastYieldedIds = eventIds; yield [...events];}
          }
        }
        if (events.length >= limit || this.aborted()) {break;}
        const next = await this.nextCursor(
          cursor.iteratorIndex,
          iterators[cursor.iteratorIndex]!,
          options,
        ).catch((error) => {
          console.warn(warning, error);
          return null;
        });
        if (next) {ready.push(next);}
      }
    } finally {
      fastProbe?.cancel();
      await Promise.all(iterators.map(async (iterator) => {
        try {
          await iterator.return?.();
        } catch {
          // Cleanup must not hide a successfully streamed page.
        }
      }));
    }

    const finalIds = events.map(event => event.id).join(',');
    if (!this.aborted() && events.length > 0 && finalIds !== lastYieldedIds) {
      yield [...events];
    }
  }

  private async *streamByAuthorTime(
    authors: string[],
    limit: number,
    options: RecentTextNoteQueryOptions,
  ): AsyncGenerator<RawEvent[], void, void> {
    if (authors.length > MAX_RECENT_AUTHOR_FALLBACK_CHUNK) {
      yield* streamChunkedAuthorFeed(
        authors,
        limit,
        () => this.aborted(),
        (chunk, chunkIndex) => this.streamByAuthorTimeChunk(
          chunk,
          limit,
          options,
          chunkIndex === 0,
        ),
      );
      return;
    }
    yield* this.streamByAuthorTimeChunk(authors, limit, options, true);
  }

  private async *streamByAuthorTimeChunk(
    authors: string[],
    limit: number,
    options: RecentTextNoteQueryOptions,
    useFastProbe: boolean,
  ): AsyncGenerator<RawEvent[], void, void> {
    const source = await this.deps.getCollectionSource();
    if (this.aborted()) {return;}
    const kindSet = new Set(getRecentTextNoteKinds(options));
    const authorSet = new Set(authors);
    const iterators = authors.map((author) => this.streamAuthorTimeCandidates(
      source, author, kindSet, options,
    )[Symbol.asyncIterator]());
    yield* this.streamMerged(
      iterators,
      kindSet,
      limit,
      options,
      'Failed to stream recent posts by author time',
      useFastProbe
        ? this.createFastProbe(
            source, authorSet, limit, options, 'Failed to read fast recent post by author time',
          )
        : undefined,
    );
  }

  private async *streamAuthorTimeCandidates(
    source: CollectionSource,
    author: string,
    kindSet: Set<number>,
    options: RecentTextNoteQueryOptions,
  ): AsyncGenerator<RawEvent, void, void> {
    for await (const { key, cid } of streamTimeOrderedIndex(
      source,
      MANIFEST_BY_AUTHOR_TIME,
      `${author}:`,
      options.before,
      options.before ? undefined : AUTHOR_TIME_POST_SCAN_LIMIT,
    )) {
      if (this.aborted()) {return;}
      const { createdAt, eventId } = timeAndEventIdFromIndexKey(key);
      if (options.until !== undefined && createdAt > options.until) {continue;}
      if (!isStrictlyBeforeRecentCursor(createdAt, eventId, options.before)) {continue;}
      if (options.since !== undefined && createdAt < options.since) {break;}
      const event = await this.readEvent(cid);
      if (
        event
        && event.pubkey === author
        && kindSet.has(event.kind)
        && isWithinTime(event, options)
        && (options.includeReplies || !isFilteredReply(event))
      ) {
        yield event;
      }
    }
  }

  private async *streamByKindTimeAuthor(
    authorSet: Set<string>,
    limit: number,
    options: RecentTextNoteQueryOptions,
  ): AsyncGenerator<RawEvent[], void, void> {
    const maxScanned = Math.max(MIN_RECENT_NOTE_SCAN_LIMIT, limit * RECENT_NOTE_SCAN_MULTIPLIER);
    const kinds = getRecentTextNoteKinds(options);
    const source = await this.deps.getCollectionSource();
    if (this.aborted()) {return;}
    const iterators = kinds.map((kind) => this.streamKindTimeAuthorCandidates(
      source, authorSet, kind, maxScanned, options,
    )[Symbol.asyncIterator]());
    yield* this.streamMerged(
      iterators,
      new Set(kinds),
      limit,
      options,
      'Failed to stream recent indexed posts by kind and author',
      this.createFastProbe(
        source, authorSet, limit, options, 'Failed to read fast recent post by time',
      ),
    );
  }

  private async *streamKindTimeAuthorCandidates(
    source: CollectionSource,
    authorSet: Set<string>,
    kind: number,
    maxScanned: number,
    options: RecentTextNoteQueryOptions,
  ): AsyncGenerator<RawEvent, void, void> {
    const hasTimeBounds = options.since !== undefined || options.until !== undefined || Boolean(options.before);
    let scanned = 0;
    let bufferedCreatedAt: number | null = null;
    let bufferedEvents: RawEvent[] = [];
    for await (const { key, cid } of streamTimeOrderedIndex(
      source,
      MANIFEST_BY_KIND_TIME_AUTHOR,
      `${padKind(kind)}:`,
      options.before,
      hasTimeBounds ? undefined : maxScanned,
    )) {
      if (this.aborted()) {return;}
      const createdAt = createdAtFromKindTimeIndexKey(key);
      if (options.until !== undefined && createdAt > options.until) {continue;}
      const eventId = key.split(':').at(-1) ?? '';
      if (!isStrictlyBeforeRecentCursor(createdAt, eventId, options.before)) {continue;}
      if (options.since !== undefined && createdAt < options.since) {break;}
      if (bufferedCreatedAt !== null && createdAt !== bufferedCreatedAt) {
        for (const event of bufferedEvents.sort((left, right) => left.id.localeCompare(right.id))) {
          yield event;
        }
        bufferedEvents = [];
      }
      bufferedCreatedAt = createdAt;
      scanned += 1;
      const author = authorFromKindTimeAuthorKey(key);
      if (author && authorSet.has(author)) {
        const event = await this.readEvent(cid);
        if (event && event.kind === kind && event.pubkey === author) {bufferedEvents.push(event);}
      }
      if (scanned >= maxScanned) {break;}
    }
    for (const event of bufferedEvents.sort((left, right) => left.id.localeCompare(right.id))) {
      yield event;
    }
  }

  private async getByKindTime(
    authorSet: Set<string>,
    limit: number,
    options: RecentTextNoteQueryOptions,
  ): Promise<RawEvent[]> {
    const maxScanned = Math.max(MIN_RECENT_NOTE_SCAN_LIMIT, limit * RECENT_NOTE_SCAN_MULTIPLIER);
    const source = await this.deps.getCollectionSource();
    const eventSets = await Promise.all(getRecentTextNoteKinds(options).map(async (kind) => {
      const events: RawEvent[] = [];
      const seenIds = new Set<string>();
      let scanned = 0;
      let batch: CID[] = [];
      const flushBatch = async (): Promise<void> => {
        if (batch.length === 0 || this.aborted()) {return;}
        const decoded = await Promise.all(batch.map(async (cid) => await this.readEvent(cid)));
        batch = [];
        for (const event of decoded) {
          if (!event || seenIds.has(event.id)) {continue;}
          if (
            event.kind !== kind
            || !authorSet.has(event.pubkey)
            || !isWithinTime(event, options)
            || (!options.includeReplies && isFilteredReply(event))
          ) {
            continue;
          }
          seenIds.add(event.id);
          events.push(event);
          if (events.length >= limit) {break;}
        }
      };
      const hasTimeBounds = options.since !== undefined || options.until !== undefined || Boolean(options.before);
      for await (const { key, cid } of streamTimeOrderedIndex(
        source,
        MANIFEST_BY_KIND_TIME,
        `${padKind(kind)}:`,
        options.before,
        hasTimeBounds ? undefined : maxScanned,
      )) {
        if (this.aborted()) {break;}
        const createdAt = createdAtFromKindTimeIndexKey(key);
        if (options.until !== undefined && createdAt > options.until) {continue;}
        const eventId = key.split(':').at(-1) ?? '';
        if (!isStrictlyBeforeRecentCursor(createdAt, eventId, options.before)) {continue;}
        if (options.since !== undefined && createdAt < options.since) {break;}
        batch.push(cid);
        scanned += 1;
        if (batch.length >= RECENT_NOTE_SCAN_BATCH) {await flushBatch();}
        if (events.length >= limit || scanned >= maxScanned) {break;}
      }
      await flushBatch();
      return events;
    }));
    return mergeRecentEvents(limit, ...eventSets);
  }
}
