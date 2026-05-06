import { describe, expect, it } from 'vitest';
import {
  PeerSelector,
  buildHedgedWavePlan,
  normalizeDispatchConfig,
} from '../src/peerSelector.js';
import {
  createRequest,
  createResponse,
  createPubsubFrame,
  createPubsubInterest,
  createPubsubInventory,
  createPubsubWant,
  encodeRequest,
  encodeResponse,
  encodePubsubFrame,
  encodePubsubInterest,
  encodePubsubInventory,
  encodePubsubWant,
  hashToKey,
  isFragmented,
  parseMessage,
  verifyHash,
} from '../src/protocol.js';

function filledHash(byte: number): Uint8Array {
  return new Uint8Array(32).fill(byte);
}

describe('@hashtree/mesh protocol', () => {
  it('round-trips requests and responses', async () => {
    const hash = filledHash(0xab);
    const data = new Uint8Array([1, 2, 3, 4]);

    const parsedRequest = parseMessage(encodeRequest(createRequest(hash, 7)));
    const parsedResponse = parseMessage(encodeResponse(createResponse(hash, data)));

    expect(parsedRequest).toEqual({
      type: 0x00,
      body: { h: hash, htl: 7 },
    });
    expect(parsedResponse).toEqual({
      type: 0x01,
      body: { h: hash, d: data },
    });
    expect(isFragmented(parsedResponse!.body)).toBe(false);
    expect(hashToKey(hash)).toBe('ab'.repeat(32));
    await expect(verifyHash(data, hash)).resolves.toBe(false);
  });

  it('round-trips pubsub inventory-first messages', () => {
    const payload = new Uint8Array([1, 2, 3]);

    expect(parseMessage(encodePubsubInterest(
      createPubsubInterest('author:alice', 'subscriber-a', 42, true, 5),
    ))).toEqual({
      type: 0x08,
      body: { s: 'author:alice', sub: 'subscriber-a', q: 42, a: true, htl: 5 },
    });
    expect(parseMessage(encodePubsubFrame(
      createPubsubFrame('author:alice', 7, 'publisher-a', payload, 4),
    ))).toEqual({
      type: 0x09,
      body: { s: 'author:alice', q: 7, o: 'publisher-a', d: payload, htl: 4 },
    });
    expect(parseMessage(encodePubsubInventory(
      createPubsubInventory('author:alice', 7, 'publisher-a', payload.byteLength, 4),
    ))).toEqual({
      type: 0x0a,
      body: { s: 'author:alice', q: 7, o: 'publisher-a', b: 3, htl: 4 },
    });
    expect(parseMessage(encodePubsubWant(
      createPubsubWant('author:alice', 7, 'publisher-a'),
    ))).toEqual({
      type: 0x0b,
      body: { s: 'author:alice', q: 7, o: 'publisher-a' },
    });
  });
});

describe('@hashtree/mesh peer selector', () => {
  it('prefers the peer with better latency and success history', () => {
    const selector = PeerSelector.withStrategy('weighted');
    selector.addPeer('fast:1');
    selector.addPeer('slow:1');

    for (let i = 0; i < 4; i += 1) {
      selector.recordRequest('fast:1', 64);
      selector.recordSuccess('fast:1', 20, 2048);
      selector.recordRequest('slow:1', 64);
      selector.recordTimeout('slow:1');
    }

    expect(selector.selectPeers()[0]).toBe('fast:1');
  });

  it('normalizes dispatch config and builds hedged waves', () => {
    const normalized = normalizeDispatchConfig({
      initialFanout: 0,
      hedgeFanout: 0,
      maxFanout: 0,
      hedgeIntervalMs: 75,
    }, 4);

    expect(normalized).toEqual({
      initialFanout: 1,
      hedgeFanout: 1,
      maxFanout: 4,
      hedgeIntervalMs: 75,
    });
    expect(buildHedgedWavePlan(5, normalized)).toEqual([1, 1, 1, 1]);
  });
});
