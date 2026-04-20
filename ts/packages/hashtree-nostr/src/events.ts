import { decode, encode } from '@msgpack/msgpack';
import {
  CollectionSource,
  serializeCollectionManifestMetadata,
  type CollectionManifest,
} from '@hashtree/collection';
import { HashTree, LinkType, type CID, type Store, toHex, sha256 } from '@hashtree/core';
import {
  COLLECTION_MANIFEST_METADATA_FILE,
  collectionManifestToNostrEventManifest,
  createNostrEventCollectionWriter,
  DEFAULT_NOSTR_EVENT_COLLECTION_SOURCE_ID,
  nostrEventCollectionManifestMetadata,
  nostrEventManifestToCollectionManifest,
} from './eventCollection.js';
import {
  compareEvents,
  createdAtFromIndexKey,
  getDTag,
  isParameterizedReplaceableKind,
  isReplaceableKind,
  MANIFEST_BY_AUTHOR_KIND_TIME,
  MANIFEST_BY_AUTHOR_TIME,
  MANIFEST_BY_ID,
  MANIFEST_BY_KIND_TIME,
  MANIFEST_BY_KIND_TIME_AUTHOR,
  MANIFEST_BY_TAG,
  MANIFEST_BY_TIME,
  MANIFEST_PARAMETERIZED_REPLACEABLE,
  MANIFEST_REPLACEABLE,
  normalizeTagName,
  normalizeTagValue,
  padKind,
  parameterizedReplaceableKey,
  replaceableKey,
  retainLatestReplaceableEvents,
  tagPrefix,
} from './eventKeys.js';
import { assertStringArray, validateEventShape, validateHex64, validateKind } from './eventValidation.js';

export const NOSTR_EVENT_ENVELOPE_VERSION = 1;

export interface StoredNostrEvent {
  id: string;
  pubkey: string;
  created_at: number;
  kind: number;
  tags: string[][];
  content: string;
  sig: string;
}

export interface NostrEventManifest {
  byId: CID | null;
  byAuthorTime: CID | null;
  byAuthorKindTime: CID | null;
  byKindTime: CID | null;
  byKindTimeAuthor: CID | null;
  byTime: CID | null;
  byTag: CID | null;
  replaceable: CID | null;
  parameterizedReplaceable: CID | null;
}

export interface ListEventsOptions {
  limit?: number;
  since?: number;
  until?: number;
}

export type NostrEventQueryValue<T> = T | readonly T[];

export interface NostrEventQuery {
  ids?: NostrEventQueryValue<string>;
  authors?: NostrEventQueryValue<string>;
  kinds?: NostrEventQueryValue<number>;
  tags?: Record<string, NostrEventQueryValue<string>>;
}

interface NormalizedNostrEventQuery {
  ids: string[];
  authors: string[];
  kinds: number[];
  tags: Map<string, string[]>;
}

interface NostrEventQueryPlan {
  type: 'ids' | 'index';
  indexName?: string;
  prefixes: string[];
}

const EMPTY_NOSTR_EVENT_QUERY: NormalizedNostrEventQuery = {
  ids: [],
  authors: [],
  kinds: [],
  tags: new Map(),
};

function normalizeQueryValues<T>(value: NostrEventQueryValue<T> | undefined): T[] {
  if (value === undefined) {
    return [];
  }

  if (Array.isArray(value)) {
    return [...value];
  }

  const scalarValue = value as T;
  return [scalarValue];
}

function dedupeValues<T>(values: T[]): T[] {
  return [...new Set(values)];
}

function normalizeNostrEventQuery(query: NostrEventQuery = {}): NormalizedNostrEventQuery {
  const ids = dedupeValues(normalizeQueryValues(query.ids).map((value) => validateHex64(value, 'event id')));
  const authors = dedupeValues(normalizeQueryValues(query.authors).map((value) => validateHex64(value, 'pubkey')));
  const kinds = dedupeValues(normalizeQueryValues(query.kinds).map((value) => validateKind(value)));
  const tags = new Map<string, string[]>();

  for (const [rawTagName, rawTagValues] of Object.entries(query.tags ?? {})) {
    const tagName = normalizeTagName(rawTagName);
    const tagValues = dedupeValues(
      normalizeQueryValues(rawTagValues).map((value) => normalizeTagValue(tagName, value)),
    );
    if (tagValues.length > 0) {
      tags.set(tagName, tagValues);
    }
  }

  return { ids, authors, kinds, tags };
}

function matchesNostrEventQuery(event: StoredNostrEvent, query: NormalizedNostrEventQuery): boolean {
  if (query.ids.length > 0 && !query.ids.includes(event.id)) {
    return false;
  }
  if (query.authors.length > 0 && !query.authors.includes(event.pubkey)) {
    return false;
  }
  if (query.kinds.length > 0 && !query.kinds.includes(event.kind)) {
    return false;
  }

  for (const [tagName, tagValues] of query.tags) {
    const matchesTag = event.tags.some(([candidateTagName, candidateTagValue]) => {
      if (typeof candidateTagName !== 'string' || typeof candidateTagValue !== 'string') {
        return false;
      }

      const normalizedCandidateName = candidateTagName.toLowerCase();
      if (normalizedCandidateName !== tagName) {
        return false;
      }

      return tagValues.includes(normalizeTagValue(tagName, candidateTagValue));
    });

    if (!matchesTag) {
      return false;
    }
  }

  return true;
}

function selectNostrEventQueryPlan(query: NormalizedNostrEventQuery): NostrEventQueryPlan {
  if (query.ids.length > 0) {
    return { type: 'ids', prefixes: [] };
  }

  if (query.authors.length > 0) {
    if (query.kinds.length === 1) {
      const kindPrefix = `${padKind(query.kinds[0]!)}:`;
      return {
        type: 'index',
        indexName: MANIFEST_BY_AUTHOR_KIND_TIME,
        prefixes: query.authors.map((author) => `${author}:${kindPrefix}`),
      };
    }

    return {
      type: 'index',
      indexName: MANIFEST_BY_AUTHOR_TIME,
      prefixes: query.authors.map((author) => `${author}:`),
    };
  }

  if (query.kinds.length > 0) {
    return {
      type: 'index',
      indexName: MANIFEST_BY_KIND_TIME,
      prefixes: query.kinds.map((kind) => `${padKind(kind)}:`),
    };
  }

  if (query.tags.size === 1) {
    const firstTag = query.tags.entries().next().value;
    if (firstTag) {
      const [tagName, tagValues] = firstTag;
      return {
        type: 'index',
        indexName: MANIFEST_BY_TAG,
        prefixes: tagValues.map((tagValue) => tagPrefix(tagName, tagValue)),
      };
    }
  }

  return {
    type: 'index',
    indexName: MANIFEST_BY_TIME,
    prefixes: [''],
  };
}

function mergeQueriedEvents(eventGroups: StoredNostrEvent[][], limit?: number): StoredNostrEvent[] {
  const merged = new Map<string, StoredNostrEvent>();

  for (const group of eventGroups) {
    for (const event of group) {
      const current = merged.get(event.id);
      if (!current || compareEvents(event, current) > 0) {
        merged.set(event.id, event);
      }
    }
  }

  const ordered = [...merged.values()].sort((left, right) => compareEvents(right, left));
  return limit === undefined ? ordered : ordered.slice(0, limit);
}

function canonicalEventIdPayload(event: Omit<StoredNostrEvent, 'id' | 'sig'>): string {
  return JSON.stringify([0, event.pubkey, event.created_at, event.kind, event.tags, event.content]);
}

async function computeCanonicalEventId(event: Omit<StoredNostrEvent, 'sig'>): Promise<string> {
  const payload = canonicalEventIdPayload(event);
  return toHex(await sha256(new TextEncoder().encode(payload)));
}

export function encodeStoredNostrEventMsgpack(event: StoredNostrEvent): Uint8Array {
  const normalized = validateEventShape(event);
  return encode([
    NOSTR_EVENT_ENVELOPE_VERSION,
    normalized.id,
    normalized.pubkey,
    normalized.created_at,
    normalized.kind,
    normalized.tags,
    normalized.content,
    normalized.sig,
  ]);
}

export function decodeStoredNostrEventMsgpack(data: Uint8Array): StoredNostrEvent {
  const decoded = decode(data);
  if (!Array.isArray(decoded) || decoded.length !== 8) {
    throw new Error('Invalid Nostr event envelope');
  }

  const [
    version,
    id,
    pubkey,
    createdAt,
    kind,
    tags,
    content,
    sig,
  ] = decoded;

  if (version !== NOSTR_EVENT_ENVELOPE_VERSION) {
    throw new Error(`Unsupported Nostr event envelope version: ${String(version)}`);
  }
  if (typeof id !== 'string' || typeof pubkey !== 'string' || typeof content !== 'string' || typeof sig !== 'string') {
    throw new Error('Invalid Nostr event envelope fields');
  }
  if (typeof createdAt !== 'number' || !Number.isInteger(createdAt) || createdAt < 0) {
    throw new Error('Invalid Nostr event created_at');
  }
  if (typeof kind !== 'number' || !Number.isInteger(kind) || kind < 0) {
    throw new Error('Invalid Nostr event kind');
  }

  assertStringArray(tags);

  return validateEventShape({
    id,
    pubkey,
    created_at: createdAt,
    kind,
    tags,
    content,
    sig,
  });
}

export class NostrEventStore {
  private readonly store: Store;
  private readonly tree: HashTree;

  constructor(store: Store) {
    this.store = store;
    this.tree = new HashTree({ store });
  }

  encodeEvent(event: StoredNostrEvent): Uint8Array {
    return encodeStoredNostrEventMsgpack(this.validateEventShape(event));
  }

  decodeEvent(data: Uint8Array): StoredNostrEvent {
    return this.validateEventShape(decodeStoredNostrEventMsgpack(data));
  }

  async add(root: CID | null, event: StoredNostrEvent): Promise<CID> {
    const normalized = await this.validateEvent(event);
    const manifest = await this.getManifest(root);
    const decision = await this.resolveReplaceableDecision(manifest, normalized);
    if (!decision.accept) {
      if (!root) {
        throw new Error('Rejecting replaceable event without an existing manifest root');
      }
      return root;
    }

    const eventBytes = this.encodeEvent(normalized);
    const { cid: eventCid } = await this.tree.putFile(eventBytes);
    const writer = this.collectionWriterFromManifest(manifest);
    await writer.put(normalized, eventCid, {
      previous: decision.replaced?.event,
    });

    const nextManifest = collectionManifestToNostrEventManifest(writer.manifest());
    const manifestRoot = await this.writeManifest(nextManifest);
    if (!manifestRoot) {
      throw new Error('Failed to create Nostr event manifest');
    }

    if (decision.replaced) {
      await this.store.delete(decision.replaced.cid.hash);
    }

    return manifestRoot;
  }

  async build(root: CID | null, events: StoredNostrEvent[]): Promise<CID | null> {
    const normalized = retainLatestReplaceableEvents(
      await Promise.all(events.map((event) => this.validateEvent(event))),
    );
    normalized.sort(compareEvents);

    if (normalized.length === 0) {
      return root;
    }

    if (root) {
      let current = root;
      for (const event of normalized) {
        current = await this.add(current, event);
      }
      return current;
    }

    const writer = this.collectionWriterFromManifest(this.emptyManifest());
    await writer.rebuild(await Promise.all(normalized.map(async (event) => {
      const { cid } = await this.tree.putFile(this.encodeEvent(event));
      return { item: event, cid };
    })));

    return await this.writeManifest(collectionManifestToNostrEventManifest(writer.manifest()));
  }

  async getById(root: CID | null, eventId: string): Promise<StoredNostrEvent | null> {
    const source = this.collectionSourceFromManifest(await this.getManifest(root));
    const eventCid = await source.get(validateHex64(eventId, 'event id'));
    if (!eventCid) {
      return null;
    }

    return this.readStoredEvent(eventCid);
  }

  async listByAuthor(root: CID | null, pubkey: string, options: ListEventsOptions = {}): Promise<StoredNostrEvent[]> {
    return this.collectEvents(
      this.collectionSourceFromManifest(await this.getManifest(root)),
      MANIFEST_BY_AUTHOR_TIME,
      `${validateHex64(pubkey, 'pubkey')}:`,
      options,
    );
  }

  async listByAuthorAndKind(
    root: CID | null,
    pubkey: string,
    kind: number,
    options: ListEventsOptions = {}
  ): Promise<StoredNostrEvent[]> {
    return this.collectEvents(
      this.collectionSourceFromManifest(await this.getManifest(root)),
      MANIFEST_BY_AUTHOR_KIND_TIME,
      `${validateHex64(pubkey, 'pubkey')}:${validateKind(kind).toString(16).padStart(8, '0')}:`,
      options,
    );
  }

  async *streamByAuthor(
    root: CID | null,
    pubkey: string,
    options: ListEventsOptions = {},
  ): AsyncGenerator<StoredNostrEvent> {
    yield* this.streamEvents(
      this.collectionSourceFromManifest(await this.getManifest(root)),
      MANIFEST_BY_AUTHOR_TIME,
      `${validateHex64(pubkey, 'pubkey')}:`,
      options,
    );
  }

  async *streamByAuthorAndKind(
    root: CID | null,
    pubkey: string,
    kind: number,
    options: ListEventsOptions = {},
  ): AsyncGenerator<StoredNostrEvent> {
    yield* this.streamEvents(
      this.collectionSourceFromManifest(await this.getManifest(root)),
      MANIFEST_BY_AUTHOR_KIND_TIME,
      `${validateHex64(pubkey, 'pubkey')}:${validateKind(kind).toString(16).padStart(8, '0')}:`,
      options,
    );
  }

  async query(
    root: CID | null,
    query: NostrEventQuery = {},
    options: ListEventsOptions = {},
  ): Promise<StoredNostrEvent[]> {
    const normalizedQuery = normalizeNostrEventQuery(query);
    const plan = selectNostrEventQueryPlan(normalizedQuery);

    if (plan.type === 'ids') {
      return this.queryByIds(root, normalizedQuery, options);
    }

    const source = this.collectionSourceFromManifest(await this.getManifest(root));
    const groups = await Promise.all(
      plan.prefixes.map((prefix) => this.collectEvents(
        source,
        plan.indexName!,
        prefix,
        options,
        normalizedQuery,
      )),
    );

    return mergeQueriedEvents(groups, options.limit);
  }

  async *streamQuery(
    root: CID | null,
    query: NostrEventQuery = {},
    options: ListEventsOptions = {},
  ): AsyncGenerator<StoredNostrEvent> {
    const normalizedQuery = normalizeNostrEventQuery(query);
    const plan = selectNostrEventQueryPlan(normalizedQuery);

    if (plan.type === 'ids' || plan.prefixes.length !== 1) {
      for (const event of await this.query(root, query, options)) {
        yield event;
      }
      return;
    }

    yield* this.streamEvents(
      this.collectionSourceFromManifest(await this.getManifest(root)),
      plan.indexName!,
      plan.prefixes[0] ?? '',
      options,
      normalizedQuery,
    );
  }

  async getReplaceable(root: CID | null, pubkey: string, kind: number): Promise<StoredNostrEvent | null> {
    const source = this.collectionSourceFromManifest(await this.getManifest(root));
    const eventCid = await source.getIndexLink(
      MANIFEST_REPLACEABLE,
      replaceableKey(validateHex64(pubkey, 'pubkey'), validateKind(kind)),
    );

    return eventCid ? this.readStoredEvent(eventCid) : null;
  }

  async listRecent(root: CID | null, options: ListEventsOptions = {}): Promise<StoredNostrEvent[]> {
    return this.collectEvents(
      this.collectionSourceFromManifest(await this.getManifest(root)),
      MANIFEST_BY_TIME,
      '',
      options,
    );
  }

  async *streamRecent(
    root: CID | null,
    options: ListEventsOptions = {},
  ): AsyncGenerator<StoredNostrEvent> {
    yield* this.streamEvents(
      this.collectionSourceFromManifest(await this.getManifest(root)),
      MANIFEST_BY_TIME,
      '',
      options,
    );
  }

  async listByTag(
    root: CID | null,
    tagName: string,
    tagValue: string,
    options: ListEventsOptions = {}
  ): Promise<StoredNostrEvent[]> {
    return this.collectEvents(
      this.collectionSourceFromManifest(await this.getManifest(root)),
      MANIFEST_BY_TAG,
      tagPrefix(tagName, tagValue),
      options,
    );
  }

  async *streamByTag(
    root: CID | null,
    tagName: string,
    tagValue: string,
    options: ListEventsOptions = {},
  ): AsyncGenerator<StoredNostrEvent> {
    yield* this.streamEvents(
      this.collectionSourceFromManifest(await this.getManifest(root)),
      MANIFEST_BY_TAG,
      tagPrefix(tagName, tagValue),
      options,
    );
  }

  async getParameterizedReplaceable(
    root: CID | null,
    pubkey: string,
    kind: number,
    dTag: string
  ): Promise<StoredNostrEvent | null> {
    if (dTag.length === 0) {
      throw new Error('Parameterized replaceable events require a non-empty d tag');
    }

    const source = this.collectionSourceFromManifest(await this.getManifest(root));
    const eventCid = await source.getIndexLink(
      MANIFEST_PARAMETERIZED_REPLACEABLE,
      parameterizedReplaceableKey(
        validateHex64(pubkey, 'pubkey'),
        validateKind(kind),
        dTag,
      ),
    );

    return eventCid ? this.readStoredEvent(eventCid) : null;
  }

  async getManifest(root: CID | null): Promise<NostrEventManifest> {
    if (!root) {
      return {
        byId: null,
        byAuthorTime: null,
        byAuthorKindTime: null,
        byKindTime: null,
        byKindTimeAuthor: null,
        byTime: null,
        byTag: null,
        replaceable: null,
        parameterizedReplaceable: null,
      };
    }

    const entries = await this.tree.listDirectory(root);
    const getCid = (name: string): CID | null => entries.find(entry => entry.name === name)?.cid ?? null;

    return {
      byId: getCid(MANIFEST_BY_ID),
      byAuthorTime: getCid(MANIFEST_BY_AUTHOR_TIME),
      byAuthorKindTime: getCid(MANIFEST_BY_AUTHOR_KIND_TIME),
      byKindTime: getCid(MANIFEST_BY_KIND_TIME),
      byKindTimeAuthor: getCid(MANIFEST_BY_KIND_TIME_AUTHOR),
      byTime: getCid(MANIFEST_BY_TIME),
      byTag: getCid(MANIFEST_BY_TAG),
      replaceable: getCid(MANIFEST_REPLACEABLE),
      parameterizedReplaceable: getCid(MANIFEST_PARAMETERIZED_REPLACEABLE),
    };
  }

  async getCollectionManifest(
    root: CID | null,
    sourceId: string = DEFAULT_NOSTR_EVENT_COLLECTION_SOURCE_ID,
  ): Promise<CollectionManifest> {
    const manifest = nostrEventManifestToCollectionManifest(await this.getManifest(root), sourceId);
    const source = new CollectionSource(this.store, manifest);
    return {
      ...manifest,
      itemCount: await source.count(),
    };
  }

  async getCollectionSource(
    root: CID | null,
    sourceId: string = DEFAULT_NOSTR_EVENT_COLLECTION_SOURCE_ID,
  ): Promise<CollectionSource> {
    return new CollectionSource(this.store, await this.getCollectionManifest(root, sourceId));
  }

  async listByKind(
    root: CID | null,
    kind: number,
    options: ListEventsOptions = {}
  ): Promise<StoredNostrEvent[]> {
    return this.collectEvents(
      this.collectionSourceFromManifest(await this.getManifest(root)),
      MANIFEST_BY_KIND_TIME,
      `${validateKind(kind).toString(16).padStart(8, '0')}:`,
      options,
    );
  }

  async *streamByKind(
    root: CID | null,
    kind: number,
    options: ListEventsOptions = {},
  ): AsyncGenerator<StoredNostrEvent> {
    yield* this.streamEvents(
      this.collectionSourceFromManifest(await this.getManifest(root)),
      MANIFEST_BY_KIND_TIME,
      `${validateKind(kind).toString(16).padStart(8, '0')}:`,
      options,
    );
  }

  private async collectEvents(
    source: CollectionSource,
    indexName: string,
    prefix: string,
    options: ListEventsOptions = {},
    query: NormalizedNostrEventQuery = EMPTY_NOSTR_EVENT_QUERY,
  ): Promise<StoredNostrEvent[]> {
    const events: StoredNostrEvent[] = [];
    const entries = indexName === MANIFEST_BY_ID
      ? await source.queryById({
        prefix,
        limit: options.limit !== undefined && options.since === undefined && options.until === undefined
          ? options.limit
          : undefined,
      })
      : await source.queryIndex(indexName, {
        prefix,
        limit: options.limit !== undefined && options.since === undefined && options.until === undefined
          ? options.limit
          : undefined,
      });

    for (const { key, cid: eventCid } of entries) {
      const createdAt = createdAtFromIndexKey(key);
      if (options.until !== undefined && createdAt > options.until) {
        continue;
      }
      if (options.since !== undefined && createdAt < options.since) {
        break;
      }
      const event = await this.tryReadStoredEvent(eventCid);
      if (!event) {
        continue;
      }
      if (!matchesNostrEventQuery(event, query)) {
        continue;
      }
      events.push(event);
      if (options.limit !== undefined && events.length >= options.limit) {
        break;
      }
    }

    return events;
  }

  private async *streamEvents(
    source: CollectionSource,
    indexName: string,
    prefix: string,
    options: ListEventsOptions = {},
    query: NormalizedNostrEventQuery = EMPTY_NOSTR_EVENT_QUERY,
  ): AsyncGenerator<StoredNostrEvent> {
    const iterator = indexName === MANIFEST_BY_ID
      ? source.streamQueryById({ prefix })
      : source.streamQueryIndex(indexName, { prefix });
    let emitted = 0;

    for await (const { key, cid: eventCid } of iterator) {
      const createdAt = createdAtFromIndexKey(key);
      if (options.until !== undefined && createdAt > options.until) {
        continue;
      }
      if (options.since !== undefined && createdAt < options.since) {
        break;
      }

      const event = await this.tryReadStoredEvent(eventCid);
      if (!event) {
        continue;
      }
      if (!matchesNostrEventQuery(event, query)) {
        continue;
      }

      yield event;
      emitted += 1;
      if (options.limit !== undefined && emitted >= options.limit) {
        break;
      }
    }
  }

  private async queryByIds(
    root: CID | null,
    query: NormalizedNostrEventQuery,
    options: ListEventsOptions = {},
  ): Promise<StoredNostrEvent[]> {
    const events: StoredNostrEvent[] = [];
    for (const eventId of query.ids) {
      const event = await this.getById(root, eventId);
      if (!event) {
        continue;
      }
      if (options.until !== undefined && event.created_at > options.until) {
        continue;
      }
      if (options.since !== undefined && event.created_at < options.since) {
        continue;
      }
      if (!matchesNostrEventQuery(event, query)) {
        continue;
      }
      events.push(event);
    }

    events.sort((left, right) => compareEvents(right, left));
    return options.limit === undefined ? events : events.slice(0, options.limit);
  }

  private async readStoredEvent(eventCid: CID): Promise<StoredNostrEvent> {
    const data = await this.tree.readFile(eventCid);
    if (!data) {
      throw new Error('Stored Nostr event blob is missing');
    }

    return this.decodeEvent(data);
  }

  private async tryReadStoredEvent(eventCid: CID): Promise<StoredNostrEvent | null> {
    try {
      return await this.readStoredEvent(eventCid);
    } catch (error) {
      if (error instanceof Error && error.message === 'Stored Nostr event blob is missing') {
        return null;
      }
      console.warn('Skipping unreadable stored Nostr event', error);
      return null;
    }
  }

  private emptyManifest(): NostrEventManifest {
    return {
      byId: null,
      byAuthorTime: null,
      byAuthorKindTime: null,
      byKindTime: null,
      byKindTimeAuthor: null,
      byTime: null,
      byTag: null,
      replaceable: null,
      parameterizedReplaceable: null,
    };
  }

  private collectionSourceFromManifest(manifest: NostrEventManifest): CollectionSource {
    return new CollectionSource(
      this.store,
      nostrEventManifestToCollectionManifest(manifest),
    );
  }

  private collectionWriterFromManifest(manifest: NostrEventManifest) {
    return createNostrEventCollectionWriter(this.store, manifest);
  }

  private async resolveReplaceableDecision(
    manifest: NostrEventManifest,
    event: StoredNostrEvent,
  ): Promise<{ accept: boolean; replaced?: { event: StoredNostrEvent; cid: CID } }> {
    const slot = isReplaceableKind(event.kind)
      ? {
        indexName: MANIFEST_REPLACEABLE,
        key: replaceableKey(event.pubkey, event.kind),
      }
      : isParameterizedReplaceableKind(event.kind)
        ? {
          indexName: MANIFEST_PARAMETERIZED_REPLACEABLE,
          key: parameterizedReplaceableKey(event.pubkey, event.kind, getDTag(event) ?? ''),
        }
        : null;

    if (!slot) {
      return { accept: true };
    }

    const source = this.collectionSourceFromManifest(manifest);
    const existingCid = await source.getIndexLink(slot.indexName, slot.key);
    if (!existingCid) {
      return { accept: true };
    }

    try {
      const existingEvent = await this.readStoredEvent(existingCid);
      if (compareEvents(event, existingEvent) > 0) {
        return {
          accept: true,
          replaced: {
            event: existingEvent,
            cid: existingCid,
          },
        };
      }
      return { accept: false };
    } catch (error) {
      if (error instanceof Error && error.message === 'Stored Nostr event blob is missing') {
        return { accept: true };
      }
      throw error;
    }
  }

  private async writeManifest(manifest: NostrEventManifest): Promise<CID | null> {
    const entries = [];
    const rootMetadata = nostrEventCollectionManifestMetadata(manifest);

    if (manifest.byId) {
      entries.push({ name: MANIFEST_BY_ID, cid: manifest.byId, size: 0, type: LinkType.Dir });
    }
    if (manifest.byAuthorTime) {
      entries.push({ name: MANIFEST_BY_AUTHOR_TIME, cid: manifest.byAuthorTime, size: 0, type: LinkType.Dir });
    }
    if (manifest.byAuthorKindTime) {
      entries.push({ name: MANIFEST_BY_AUTHOR_KIND_TIME, cid: manifest.byAuthorKindTime, size: 0, type: LinkType.Dir });
    }
    if (manifest.byKindTime) {
      entries.push({ name: MANIFEST_BY_KIND_TIME, cid: manifest.byKindTime, size: 0, type: LinkType.Dir });
    }
    if (manifest.byKindTimeAuthor) {
      entries.push({ name: MANIFEST_BY_KIND_TIME_AUTHOR, cid: manifest.byKindTimeAuthor, size: 0, type: LinkType.Dir });
    }
    if (manifest.byTime) {
      entries.push({ name: MANIFEST_BY_TIME, cid: manifest.byTime, size: 0, type: LinkType.Dir });
    }
    if (manifest.byTag) {
      entries.push({ name: MANIFEST_BY_TAG, cid: manifest.byTag, size: 0, type: LinkType.Dir });
    }
    if (manifest.replaceable) {
      entries.push({ name: MANIFEST_REPLACEABLE, cid: manifest.replaceable, size: 0, type: LinkType.Dir });
    }
    if (manifest.parameterizedReplaceable) {
      entries.push({
        name: MANIFEST_PARAMETERIZED_REPLACEABLE,
        cid: manifest.parameterizedReplaceable,
        size: 0,
        type: LinkType.Dir,
      });
    }
    if (rootMetadata) {
      const metadataBytes = serializeCollectionManifestMetadata(rootMetadata);
      const { cid, size } = await this.tree.putFile(metadataBytes);
      entries.push({
        name: COLLECTION_MANIFEST_METADATA_FILE,
        cid,
        size,
        type: LinkType.File,
      });
    }

    if (entries.length === 0) {
      return null;
    }

    const { cid } = await this.tree.putDirectory(entries);
    return cid;
  }

  private validateEventShape(event: StoredNostrEvent): StoredNostrEvent {
    return validateEventShape(event);
  }

  private async validateEvent(event: StoredNostrEvent): Promise<StoredNostrEvent> {
    const normalized = this.validateEventShape(event);
    const computedId = await computeCanonicalEventId(normalized);
    if (computedId !== normalized.id) {
      throw new Error(`Event id mismatch: expected ${computedId}, got ${normalized.id}`);
    }

    return normalized;
  }
}
