import { describe, expect, it } from 'vitest';
import { HashTree, MemoryStore, sha256, toHex } from '@hashtree/core';
import { loadCollectionManifestMetadata } from '@hashtree/collection';
import {
  COLLECTION_MANIFEST_METADATA_FILE,
  decodeStoredNostrEventMsgpack,
  encodeStoredNostrEventMsgpack,
  NostrEventStore,
  type StoredNostrEvent,
} from '../src/index.js';

async function makeEvent(
  overrides: Partial<Omit<StoredNostrEvent, 'id'>> & { id?: string } = {}
): Promise<StoredNostrEvent> {
  const pubkey = overrides.pubkey ?? '1'.repeat(64);
  const createdAt = overrides.created_at ?? 1_700_000_000;
  const kind = overrides.kind ?? 1;
  const tags = overrides.tags ?? [];
  const content = overrides.content ?? 'hello world';
  const idPayload = JSON.stringify([0, pubkey, createdAt, kind, tags, content]);
  const id = overrides.id ?? toHex(await sha256(new TextEncoder().encode(idPayload)));

  return {
    id,
    pubkey,
    created_at: createdAt,
    kind,
    tags,
    content,
    sig: overrides.sig ?? '2'.repeat(128),
  };
}

describe('NostrEventStore', () => {
  it('encodes deterministically and decodes full events', async () => {
    const store = new NostrEventStore(new MemoryStore());
    const event = await makeEvent({
      kind: 7,
      content: 'deterministic payload',
      tags: [['e', 'a'.repeat(64)], ['p', 'b'.repeat(64)]],
      sig: '3'.repeat(128),
    });

    const encodedA = store.encodeEvent(event);
    const encodedB = store.encodeEvent({ ...event });

    expect(encodedA).toEqual(encodedB);
    expect(store.decodeEvent(encodedA)).toEqual(event);
    expect(decodeStoredNostrEventMsgpack(encodeStoredNostrEventMsgpack(event))).toEqual(event);
  });

  it('stores events by id and exposes manifest roots', async () => {
    const store = new NostrEventStore(new MemoryStore());
    const event = await makeEvent();

    const root = await store.add(null, event);
    const manifest = await store.getManifest(root);

    expect(manifest.byId).not.toBeNull();
    expect(manifest.byAuthorTime).not.toBeNull();
    expect(manifest.byAuthorKindTime).not.toBeNull();
    expect(manifest.byKindTime).not.toBeNull();
    expect(manifest.byKindTimeAuthor).not.toBeNull();

    await expect(store.getById(root, event.id)).resolves.toEqual(event);
    await expect(store.getById(root, 'f'.repeat(64))).resolves.toBeNull();
  });

  it('exposes only the by-id manifest key', async () => {
    const backing = new MemoryStore();
    const store = new NostrEventStore(backing);
    const tree = new HashTree({ store: backing });
    const event = await makeEvent();

    const root = await store.add(null, event);
    const entries = await tree.listDirectory(root);
    const names = entries.map(entry => entry.name);

    expect(names).toContain('by-id');
    expect(names).not.toContain('events_by_id');
  });

  it('publishes shared collection manifest metadata alongside event indexes', async () => {
    const backing = new MemoryStore();
    const store = new NostrEventStore(backing);
    const tree = new HashTree({ store: backing });
    const event = await makeEvent();

    const root = await store.add(null, event);
    const entries = await tree.listDirectory(root);

    expect(entries.map((entry) => entry.name)).toContain(COLLECTION_MANIFEST_METADATA_FILE);
    await expect(loadCollectionManifestMetadata(backing, root)).resolves.toEqual({
      version: 1,
      schemaVersion: 1,
      publishedSchema: {
        itemFormat: 'nostr/event@1',
        projectionFormat: 'hashtree/nostr-event-index@1',
      },
    });
  });

  it('lists author feeds newest first', async () => {
    const store = new NostrEventStore(new MemoryStore());
    const author = 'a'.repeat(64);
    const otherAuthor = 'b'.repeat(64);
    const older = await makeEvent({ pubkey: author, created_at: 10, content: 'older', kind: 1 });
    const newest = await makeEvent({ pubkey: author, created_at: 30, content: 'newest', kind: 1 });
    const middle = await makeEvent({ pubkey: author, created_at: 20, content: 'middle', kind: 7 });
    const other = await makeEvent({ pubkey: otherAuthor, created_at: 40, content: 'other', kind: 1 });

    let root = await store.add(null, older);
    root = await store.add(root, newest);
    root = await store.add(root, middle);
    root = await store.add(root, other);

    await expect(store.listByAuthor(root, author)).resolves.toEqual([newest, middle, older]);
    await expect(store.listByAuthorAndKind(root, author, 1)).resolves.toEqual([newest, older]);
    await expect(store.listByKind(root, 1)).resolves.toEqual([other, newest, older]);
    await expect(store.listRecent(root, { limit: 3 })).resolves.toEqual([other, newest, middle]);
    await expect(store.listRecent(root, { since: 20, until: 40 })).resolves.toEqual([
      other,
      newest,
      middle,
    ]);
  });

  it('queries feeds with hashtree-side filter planning instead of app-side index glue', async () => {
    const store = new NostrEventStore(new MemoryStore());
    const author = 'a'.repeat(64);
    const otherAuthor = 'b'.repeat(64);
    const older = await makeEvent({
      pubkey: author,
      created_at: 10,
      content: 'older tagged',
      tags: [['t', 'nostr']],
    });
    const newest = await makeEvent({
      pubkey: author,
      created_at: 30,
      content: 'newest tagged',
      tags: [['t', 'hashtree']],
    });
    const other = await makeEvent({
      pubkey: otherAuthor,
      created_at: 40,
      content: 'other tagged',
      tags: [['t', 'nostr']],
    });

    let root = await store.add(null, older);
    root = await store.add(root, newest);
    root = await store.add(root, other);

    await expect(store.query(root, { authors: author, kinds: 1 })).resolves.toEqual([newest, older]);
    await expect(store.query(root, { authors: [author, otherAuthor], kinds: 1 })).resolves.toEqual([
      other,
      newest,
      older,
    ]);
    await expect(store.query(root, { tags: { t: 'nostr' } })).resolves.toEqual([other, older]);
    await expect(store.query(root, { ids: [older.id, newest.id] }, { limit: 1 })).resolves.toEqual([newest]);
  });

  it('applies limits after residual query predicates', async () => {
    const store = new NostrEventStore(new MemoryStore());
    const author = 'a'.repeat(64);
    const matching = await makeEvent({
      pubkey: author,
      created_at: 10,
      kind: 1,
      tags: [['t', 'wanted']],
    });
    const newerNonMatching = await makeEvent({
      pubkey: author,
      created_at: 20,
      kind: 1,
      tags: [['t', 'other']],
    });

    const root = await store.build(null, [matching, newerNonMatching]);

    await expect(store.query(root, {
      authors: author,
      kinds: 1,
      tags: { t: 'wanted' },
    }, { limit: 1 })).resolves.toEqual([matching]);
  });

  it('streams planned queries for direct per-author lookups', async () => {
    const store = new NostrEventStore(new MemoryStore());
    const author = 'f'.repeat(64);
    const older = await makeEvent({ pubkey: author, created_at: 10, content: 'older' });
    const newer = await makeEvent({ pubkey: author, created_at: 20, content: 'newer' });

    let root = await store.add(null, older);
    root = await store.add(root, newer);

    const streamed: StoredNostrEvent[] = [];
    for await (const event of store.streamQuery(root, { authors: author, kinds: 1 }, { limit: 1 })) {
      streamed.push(event);
    }

    expect(streamed).toEqual([newer]);
  });

  it('indexes hashtag tags case-insensitively for search', async () => {
    const store = new NostrEventStore(new MemoryStore());
    const author = 'a'.repeat(64);
    const older = await makeEvent({
      pubkey: author,
      created_at: 10,
      content: 'older tagged',
      tags: [['t', 'nostr']],
    });
    const newer = await makeEvent({
      pubkey: author,
      created_at: 20,
      content: 'newer tagged',
      tags: [['t', 'Hashtree'], ['t', 'nostr']],
    });

    let root = await store.add(null, older);
    root = await store.add(root, newer);

    await expect(store.listByTag(root, 't', 'nostr')).resolves.toEqual([newer, older]);
    await expect(store.listByTag(root, 't', 'hashtree')).resolves.toEqual([newer]);
  });

  it('tracks the latest replaceable event for an author and kind', async () => {
    const store = new NostrEventStore(new MemoryStore());
    const author = 'c'.repeat(64);
    const first = await makeEvent({ pubkey: author, kind: 0, created_at: 50, content: 'first profile' });
    const latest = await makeEvent({ pubkey: author, kind: 0, created_at: 60, content: 'latest profile' });

    let root = await store.add(null, first);
    root = await store.add(root, latest);

    await expect(store.getReplaceable(root, author, 0)).resolves.toEqual(latest);
  });

  it('tracks the latest parameterized replaceable event using d tags and event id tie-breaks', async () => {
    const author = 'd'.repeat(64);
    const first = await makeEvent({
      pubkey: author,
      kind: 30_023,
      created_at: 70,
      content: 'draft alpha',
      tags: [['d', 'article-1']],
    });
    const second = await makeEvent({
      pubkey: author,
      kind: 30_023,
      created_at: 70,
      content: 'draft omega',
      tags: [['d', 'article-1']],
    });
    const lowest = first.id < second.id ? first : second;
    const highest = lowest === first ? second : first;

    const highFirstStore = new NostrEventStore(new MemoryStore());
    let highFirstRoot = await highFirstStore.add(null, highest);
    highFirstRoot = await highFirstStore.add(highFirstRoot, lowest);

    const lowFirstStore = new NostrEventStore(new MemoryStore());
    let lowFirstRoot = await lowFirstStore.add(null, lowest);
    lowFirstRoot = await lowFirstStore.add(lowFirstRoot, highest);

    await expect(
      highFirstStore.getParameterizedReplaceable(highFirstRoot, author, 30_023, 'article-1')
    ).resolves.toEqual(lowest);
    await expect(
      lowFirstStore.getParameterizedReplaceable(lowFirstRoot, author, 30_023, 'article-1')
    ).resolves.toEqual(lowest);
    expect(highFirstRoot).toEqual(lowFirstRoot);
  });

  it('reads an addressable event whose d-tag value is empty', async () => {
    const store = new NostrEventStore(new MemoryStore());
    const author = 'd'.repeat(64);
    const event = await makeEvent({
      pubkey: author,
      kind: 30_023,
      created_at: 75,
      content: 'default article',
      tags: [['d', '']],
    });

    const root = await store.add(null, event);

    await expect(store.getParameterizedReplaceable(root, author, 30_023, '')).resolves.toEqual(event);
  });

  it('builds the same replaceable index root with the lowest same-second id in either order', async () => {
    const author = 'e'.repeat(64);
    const first = await makeEvent({
      pubkey: author,
      kind: 0,
      created_at: 80,
      content: 'profile alpha',
    });
    const second = await makeEvent({
      pubkey: author,
      kind: 0,
      created_at: 80,
      content: 'profile omega',
    });
    const lowest = first.id < second.id ? first : second;
    const highest = lowest === first ? second : first;

    const highFirstStore = new NostrEventStore(new MemoryStore());
    const highFirstRoot = await highFirstStore.build(null, [highest, lowest]);
    const lowFirstStore = new NostrEventStore(new MemoryStore());
    const lowFirstRoot = await lowFirstStore.build(null, [lowest, highest]);

    expect(highFirstRoot).not.toBeNull();
    expect(highFirstRoot).toEqual(lowFirstRoot);
    await expect(highFirstStore.getReplaceable(highFirstRoot, author, 0)).resolves.toEqual(lowest);
    await expect(lowFirstStore.getReplaceable(lowFirstRoot, author, 0)).resolves.toEqual(lowest);
  });

  it('builds deterministic roots from unordered event sets', async () => {
    const store = new NostrEventStore(new MemoryStore());
    const author = 'a'.repeat(64);
    const older = await makeEvent({ pubkey: author, created_at: 10, content: 'older', kind: 1 });
    const newer = await makeEvent({ pubkey: author, created_at: 20, content: 'newer', kind: 1 });
    const profile = await makeEvent({
      pubkey: author,
      created_at: 30,
      content: 'profile',
      kind: 0,
      sig: '3'.repeat(128),
    });

    const built = await store.build(null, [profile, older, newer]);

    let incremental = await store.add(null, older);
    incremental = await store.add(incremental, newer);
    incremental = await store.add(incremental, profile);

    expect(built).toEqual(incremental);
  });
});
