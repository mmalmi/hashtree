import { encode } from '@msgpack/msgpack';
import { MemoryStore } from '@hashtree/core';
import { finalizeEvent, getEventHash, verifyEvent } from 'nostr-tools/pure';
import { describe, expect, it } from 'vitest';
import {
  decodeStoredNostrEventMsgpack,
  encodeStoredNostrEventMsgpack,
  NostrEventStore,
} from '../src/events.js';

const fixtureKey = new Uint8Array(32).fill(1);

function signedEvent(content: string, tags: string[][] = []) {
  const unsigned = { kind: 1, created_at: 1_700_000_000, content, tags };
  const { id, pubkey, sig } = finalizeEvent({ ...unsigned }, fixtureKey);
  return { ...unsigned, id, pubkey, sig };
}

function envelope(event: ReturnType<typeof signedEvent>): unknown[] {
  return [1, event.id, event.pubkey, event.created_at, event.kind, event.tags, event.content, event.sig];
}

describe('signed Nostr MessagePack strings', () => {
  for (const byteLength of [199, 200, 201, 253]) {
    for (const field of ['content', 'tag name', 'tag value'] as const) {
      it(`preserves leading U+FEFF in ${field} at ${byteLength} UTF-8 bytes`, () => {
        const text = '\uFEFF' + 'x'.repeat(byteLength - 3);
        expect(new TextEncoder().encode(text)).toHaveLength(byteLength);
        const event = signedEvent(field === 'content' ? text : 'signed content', [
          [field === 'tag name' ? text : 't', field === 'tag value' ? text : 'value'],
        ]);
        const decoded = decodeStoredNostrEventMsgpack(encodeStoredNostrEventMsgpack(event));

        expect(decoded).toEqual(event);
        expect(getEventHash(decoded)).toBe(event.id);
        expect(verifyEvent(decoded)).toBe(true);
      });
    }
  }

  it('preserves repeated and interior BOMs, Unicode and empty strings through stored reads', async () => {
    const store = new NostrEventStore(new MemoryStore());
    const event = signedEvent('\uFEFF\uFEFF' + '🌲 café\n'.repeat(32) + '\uFEFF', [
      ['t', '', '\uFEFF\uFEFF' + 'λ'.repeat(110), 'interior\uFEFFvalue'],
      [],
    ]);
    const root = await store.add(null, event);
    const decoded = await store.getById(root, event.id);

    expect(decoded).toEqual(event);
    expect(await store.listByAuthorAndKind(root, event.pubkey, event.kind)).toEqual([decoded]);
    expect(verifyEvent(decoded!)).toBe(true);
  });

  for (const fieldIndex of [1, 2, 6, 7]) {
    it(`rejects MessagePack binary instead of string field ${fieldIndex}`, () => {
      const encoded = envelope(signedEvent('valid'));
      encoded[fieldIndex] = new TextEncoder().encode(encoded[fieldIndex] as string);
      expect(() => decodeStoredNostrEventMsgpack(encode(encoded))).toThrow('Invalid Nostr event envelope fields');
    });
  }

  for (const member of [0, 1]) {
    it(`rejects a binary tag member at position ${member}`, () => {
      const encoded = envelope(signedEvent('valid', [['t', 'value']]));
      const tag: unknown[] = ['t', 'value'];
      tag[member] = new TextEncoder().encode(tag[member] as string);
      encoded[5] = [tag];
      expect(() => decodeStoredNostrEventMsgpack(encode(encoded))).toThrow('array of string arrays');
    });
  }

  for (const field of ['id', 'sig', 'content'] as const) {
    it(`does not make a tampered ${field} verify`, () => {
      const event = signedEvent('\uFEFF' + 'x'.repeat(250));
      const tampered = { ...event, [field]: field === 'content' ? event.content + '!' : '0'.repeat(event[field].length) };
      const decoded = decodeStoredNostrEventMsgpack(encodeStoredNostrEventMsgpack(tampered));
      expect(verifyEvent(decoded)).toBe(false);
    });
  }

  for (const field of ['content', 'tag'] as const) {
    it(`rejects malformed UTF-8 in ${field} instead of changing signed text`, () => {
      const event = signedEvent(field === 'content' ? '🌲' : 'content', [
        ['t', field === 'tag' ? '🌲' : 'value'],
      ]);
      const bytes = encodeStoredNostrEventMsgpack(event);
      const offset = bytes.indexOf(0xf0);
      expect(offset).toBeGreaterThan(0);
      bytes[offset] = 0xff;
      expect(() => decodeStoredNostrEventMsgpack(bytes)).toThrow();
    });
  }
});
