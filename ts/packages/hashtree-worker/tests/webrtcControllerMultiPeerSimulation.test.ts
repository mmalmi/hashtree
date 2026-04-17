import { afterEach, describe, expect, it, vi } from 'vitest';
import { MemoryStore, sha256 } from '@hashtree/core';
import {
  MSG_TYPE_REQUEST,
  createResponse,
  encodeResponse,
  hashToKey,
  parseMessage,
} from '@hashtree/nostr';
import { WebRTCController } from '../src/p2p/webrtcController.js';

interface ControllerPeer {
  peerId: string;
  dataChannelReady: boolean;
  state: 'connecting' | 'connected' | 'disconnected';
}

interface ControllerPrivateApi {
  createPeer: (
    peerId: string,
    pubkey: string,
    pool: 'follows' | 'other',
    direction: 'inbound' | 'outbound'
  ) => ControllerPeer;
  onDataChannelMessage: (peerId: string, data: Uint8Array) => Promise<void>;
}

interface SimulatedPeerProfile {
  payloadsByHash: Map<string, Uint8Array>;
  corrupt?: boolean;
  availableAtMs: number;
  serviceMs?: number;
  firstByteMs?: number;
  bytesPerSecond?: number;
  stallOnRequestNumbers?: number[];
  stallMs?: number;
}

function connectPeer(
  internal: ControllerPrivateApi,
  peerId: string,
  pubkey: string,
): ControllerPeer {
  const peer = internal.createPeer(peerId, pubkey, 'other', 'outbound');
  peer.state = 'connected';
  peer.dataChannelReady = true;
  return peer;
}

function createTimedController(
  peerProfiles: Record<string, SimulatedPeerProfile>,
  options?: { requestTimeout?: number },
) {
  const requestCounts = new Map<string, number>();
  const controller = new WebRTCController({
    pubkey: 'self-pubkey',
    localStore: new MemoryStore(),
    sendCommand: (cmd) => {
      if (cmd.type !== 'rtc:sendData') {
        return;
      }

      const message = parseMessage(cmd.data);
      if (message?.type !== MSG_TYPE_REQUEST) {
        return;
      }

      requestCounts.set(cmd.peerId, (requestCounts.get(cmd.peerId) ?? 0) + 1);
      const profile = peerProfiles[cmd.peerId];
      if (!profile) {
        return;
      }

      const now = Date.now();
      const startAt = Math.max(now, profile.availableAtMs);
      const payload = profile.payloadsByHash.get(hashToKey(message.body.h));
      if (!payload) {
        return;
      }

      const requestNumber = requestCounts.get(cmd.peerId) ?? 0;
      let serviceMs = profile.serviceMs ?? 0;
      if (profile.firstByteMs !== undefined || profile.bytesPerSecond !== undefined) {
        serviceMs = profile.firstByteMs ?? 0;
        if ((profile.bytesPerSecond ?? 0) > 0) {
          serviceMs += Math.ceil((payload.byteLength * 1000) / (profile.bytesPerSecond ?? 1));
        }
      }
      if (profile.stallMs && profile.stallOnRequestNumbers?.includes(requestNumber)) {
        serviceMs += profile.stallMs;
      }
      const deliverAt = startAt + serviceMs;
      profile.availableAtMs = deliverAt;

      const responsePayload = profile.corrupt
        ? new Uint8Array(payload.map((byte, idx) => (idx === 0 ? byte ^ 0xff : byte)))
        : payload;
      setTimeout(() => {
        void internal.onDataChannelMessage(
          cmd.peerId,
          new Uint8Array(encodeResponse(createResponse(message.body.h, responsePayload))),
        );
      }, Math.max(0, deliverAt - now));
    },
    sendSignaling: async () => {},
    requestTimeout: options?.requestTimeout ?? 250,
    requestDispatch: {
      initialFanout: 1,
      hedgeFanout: 1,
      maxFanout: 2,
      hedgeIntervalMs: 25,
    },
  });

  const internal = controller as unknown as ControllerPrivateApi;
  for (const peerId of Object.keys(peerProfiles)) {
    connectPeer(internal, peerId, `${peerId}-pubkey`);
  }

  return { controller, internal, requestCounts };
}

async function preparePayloads(count: number): Promise<Array<{ hash: Uint8Array; payload: Uint8Array }>> {
  const payloads: Array<{ hash: Uint8Array; payload: Uint8Array }> = [];
  for (let i = 0; i < count; i++) {
    const payload = new TextEncoder().encode(`block-${i}`);
    payloads.push({ hash: await sha256(payload), payload });
  }
  return payloads;
}

async function runConcurrentFetches(
  controller: WebRTCController,
  payloads: Array<{ hash: Uint8Array; payload: Uint8Array }>,
): Promise<{ elapsedMs: number; results: Array<Uint8Array | null> }> {
  const startedAt = Date.now();
  const settled = await Promise.all(
    payloads.map(({ hash }) =>
      controller.get(hash).then((data) => ({
        data,
        finishedAt: Date.now(),
      })),
    ),
  );
  return {
    elapsedMs: Math.max(...settled.map((entry) => entry.finishedAt)) - startedAt,
    results: settled.map((entry) => entry.data),
  };
}

describe('WebRTCController multi-peer block scheduling', () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it('finishes a block batch faster across mixed peers and avoids a poisoned fast peer', async () => {
    const payloads = await preparePayloads(6);

    const singlePeerPayloads = new Map(payloads.map(({ hash, payload }) => [hashToKey(hash), payload]));
    const baseline = createTimedController({
      'peer-fast': {
        serviceMs: 45,
        payloadsByHash: new Map(singlePeerPayloads),
        availableAtMs: 0,
      },
    });
    const baselineRun = await runConcurrentFetches(baseline.controller, payloads.slice(1));
    expect(baselineRun.results).toEqual(payloads.slice(1).map(({ payload }) => payload));

    const mixed = createTimedController({
      'peer-a-junk': {
        serviceMs: 15,
        payloadsByHash: new Map(singlePeerPayloads),
        corrupt: true,
        availableAtMs: 0,
      },
      'peer-b-fast': {
        serviceMs: 45,
        payloadsByHash: new Map(singlePeerPayloads),
        availableAtMs: 0,
      },
      'peer-c-medium': {
        serviceMs: 80,
        payloadsByHash: new Map(singlePeerPayloads),
        availableAtMs: 0,
      },
    });

    const warmup = await runConcurrentFetches(mixed.controller, [payloads[0]]);
    expect(warmup.results).toEqual([payloads[0].payload]);

    mixed.requestCounts.clear();
    const mixedRun = await runConcurrentFetches(mixed.controller, payloads.slice(1));
    expect(mixedRun.results).toEqual(payloads.slice(1).map(({ payload }) => payload));

    expect(mixed.requestCounts.get('peer-a-junk') ?? 0).toBe(0);
    expect(mixed.requestCounts.get('peer-b-fast') ?? 0).toBeGreaterThan(0);
    expect(mixed.requestCounts.get('peer-c-medium') ?? 0).toBeGreaterThan(0);
    expect(mixedRun.elapsedMs).toBeLessThan(baselineRun.elapsedMs);
  });

  it('adapts to peers with slow large-payload throughput instead of treating all hits as equal', async () => {
    const payloads = await Promise.all(
      Array.from({ length: 4 }, async (_, index) => {
        const payload = new Uint8Array(16 * 1024).fill(index + 1);
        return { hash: await sha256(payload), payload };
      }),
    );

    const payloadsByHash = new Map(payloads.map(({ hash, payload }) => [hashToKey(hash), payload]));
    const baseline = createTimedController({
      'peer-slow-link': {
        firstByteMs: 10,
        bytesPerSecond: 20_000,
        payloadsByHash: new Map(payloadsByHash),
        availableAtMs: 0,
      },
    }, { requestTimeout: 4_000 });
    const baselineRun = await runConcurrentFetches(baseline.controller, payloads);
    expect(baselineRun.results).toEqual(payloads.map(({ payload }) => payload));

    const mixed = createTimedController({
      'peer-slow-link': {
        firstByteMs: 10,
        bytesPerSecond: 20_000,
        payloadsByHash: new Map(payloadsByHash),
        availableAtMs: 0,
      },
      'peer-fast-link': {
        firstByteMs: 30,
        bytesPerSecond: 140_000,
        payloadsByHash: new Map(payloadsByHash),
        availableAtMs: 0,
      },
    }, { requestTimeout: 4_000 });
    const mixedRun = await runConcurrentFetches(mixed.controller, payloads);
    expect(mixedRun.results).toEqual(payloads.map(({ payload }) => payload));

    expect(mixed.requestCounts.get('peer-fast-link') ?? 0).toBeGreaterThan(0);
    expect(mixedRun.elapsedMs).toBeLessThan(baselineRun.elapsedMs);
  });
});
