import { describe, expect, it } from 'vitest';
import { HashTree, MemoryStore, nhashEncode } from '@hashtree/core';
import {
  decodeSignedNostrEventJson,
  encodeSignedNostrEventJson,
  parseHashtreeRootEvent,
  readSignedNostrEventSnapshot,
  storeSignedNostrEventSnapshot,
  HASHTREE_LEGACY_ROOT_KIND,
  HASHTREE_ROOT_KIND,
  type ParsedHashtreeRootEvent,
} from '../src/snapshot.js';
import type { StoredNostrEvent } from '../src/events.js';

function makeEvent(overrides: Partial<StoredNostrEvent> = {}): StoredNostrEvent {
  return {
    id: '1'.repeat(64),
    pubkey: '2'.repeat(64),
    created_at: 1_700_000_000,
    kind: HASHTREE_ROOT_KIND,
    tags: [
      ['d', 'videos/demo'],
      ['l', 'hashtree'],
      ['hash', '3'.repeat(64)],
      ['key', '4'.repeat(64)],
    ],
    content: '',
    sig: '5'.repeat(128),
    ...overrides,
  };
}

describe('Nostr event snapshots', () => {
  it('encodes signed Nostr event JSON deterministically', () => {
    const event = makeEvent();

    const encoded = encodeSignedNostrEventJson(event);

    expect(new TextDecoder().decode(encoded)).toBe(
      JSON.stringify({
        id: event.id,
        pubkey: event.pubkey,
        created_at: event.created_at,
        kind: event.kind,
        tags: event.tags,
        content: event.content,
        sig: event.sig,
      }),
    );
    expect(decodeSignedNostrEventJson(encoded)).toEqual(event);
  });

  it('stores and reloads public event snapshots as plain hashtree blobs', async () => {
    const store = new MemoryStore();
    const event = makeEvent();

    const snapshotCid = await storeSignedNostrEventSnapshot(store, event);
    const restored = await readSignedNostrEventSnapshot(store, snapshotCid);

    expect(snapshotCid.key).toBeUndefined();
    expect(restored).toEqual(event);
    expect(nhashEncode(snapshotCid)).toMatch(/^nhash1/);
  });

  it('accepts a HashTree target so caller controls write policy', async () => {
    const store = new MemoryStore();
    const tree = new HashTree({ store });
    const event = makeEvent();

    const snapshotCid = await storeSignedNostrEventSnapshot(tree, event);
    const restored = await readSignedNostrEventSnapshot(tree, snapshotCid);

    expect(restored).toEqual(event);
  });

  it('parses hashtree root events from signed event snapshots', () => {
    const parsed = parseHashtreeRootEvent(makeEvent()) as ParsedHashtreeRootEvent;

    expect(parsed.treeName).toBe('videos/demo');
    expect(parsed.visibility).toBe('public');
    expect(parsed.labels).toEqual(['hashtree']);
    expect(parsed.rootCid.key).toBeDefined();
  });

  it('parses legacy NIP-78 hashtree root events', () => {
    const parsed = parseHashtreeRootEvent(makeEvent({ kind: HASHTREE_LEGACY_ROOT_KIND }));

    expect(parsed?.treeName).toBe('videos/demo');
  });

  it('parses link-visible root events without exposing the CHK directly', () => {
    const parsed = parseHashtreeRootEvent(makeEvent({
      tags: [
        ['d', 'videos/demo'],
        ['l', 'hashtree'],
        ['hash', '3'.repeat(64)],
        ['encryptedKey', '6'.repeat(64)],
        ['keyId', '7'.repeat(64)],
      ],
    })) as ParsedHashtreeRootEvent;

    expect(parsed.visibility).toBe('link-visible');
    expect(parsed.rootCid.key).toBeUndefined();
    expect(parsed.encryptedKey).toBe('6'.repeat(64));
    expect(parsed.keyId).toBe('7'.repeat(64));
  });
});
