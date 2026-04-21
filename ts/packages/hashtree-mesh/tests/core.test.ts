import { describe, expect, it } from 'vitest';
import {
  PeerSelector,
  buildHedgedWavePlan,
  normalizeDispatchConfig,
} from '../src/peerSelector.js';
import {
  createRequest,
  createResponse,
  encodeRequest,
  encodeResponse,
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
