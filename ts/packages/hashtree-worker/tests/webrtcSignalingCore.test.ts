import { describe, expect, it, vi } from 'vitest';
import type { SignalingMessage } from '@hashtree/nostr';
import type { Event as NostrEvent } from 'nostr-tools';
import {
  SIGNALING_KIND,
  HELLO_TAG,
  createSignalingFilters,
  createSimplePoolSignalingSender,
  sendSignalingMessage,
  decodeSignalingEvent,
  type SignalingEventLike,
  type GiftSeal,
} from '../src/p2p/signaling.js';

describe('p2p signaling core', () => {
  it('builds expected filters for hello and directed messages', () => {
    const nowMs = 1700000000000;
    const filters = createSignalingFilters('pubkey-abc', nowMs);

    expect(filters.helloFilter).toEqual({
      kinds: [SIGNALING_KIND],
      '#l': [HELLO_TAG],
      since: Math.floor((nowMs - 30000) / 1000),
    });
    expect(filters.directedFilter).toEqual({
      kinds: [SIGNALING_KIND],
      '#p': ['pubkey-abc'],
      since: Math.floor((nowMs - 30000) / 1000),
    });
  });

  it('sends hello via signEvent + publish', async () => {
    const published: SignalingEventLike[] = [];
    const signEvent = vi.fn(async (template: { kind: number; created_at: number; tags: string[][]; content: string }) => ({
      pubkey: 'signer',
      created_at: template.created_at,
      tags: template.tags,
      content: template.content,
    }));
    const giftWrap = vi.fn();
    const nowMs = 1700000000000;

    const message: SignalingMessage = { type: 'hello', peerId: 'f'.repeat(64), hashGet: false };
    await sendSignalingMessage({
      msg: message,
      signEvent,
      giftWrap,
      publish: async (event) => {
        published.push(event);
      },
      nowMs: () => nowMs,
    });

    expect(giftWrap).not.toHaveBeenCalled();
    expect(signEvent).toHaveBeenCalledTimes(1);
    expect(published).toHaveLength(1);
    expect(published[0]?.tags).toContainEqual(['l', HELLO_TAG]);
    expect(published[0]?.tags).toContainEqual(['peerId', 'f'.repeat(64)]);
    expect(published[0]?.tags).toContainEqual(['hashGet', '0']);
  });

  it('sends directed signaling via giftWrap + publish', async () => {
    const published: SignalingEventLike[] = [];
    const signEvent = vi.fn();
    const giftWrap = vi.fn(async (inner: { kind: number; content: string; tags: string[][] }, _recipient: string) => ({
      pubkey: 'ephemeral',
      created_at: 1700000000,
      tags: [['p', 'recipient']],
      content: inner.content,
    }));

    const message: SignalingMessage = {
      type: 'offer',
      peerId: 'sender',
      targetPeerId: 'recipient',
      sdp: 'v=0',
    };

    await sendSignalingMessage({
      msg: message,
      recipientPubkey: 'recipient',
      signEvent,
      giftWrap,
      publish: async (event) => {
        published.push(event);
      },
    });

    expect(signEvent).not.toHaveBeenCalled();
    expect(giftWrap).toHaveBeenCalledTimes(1);
    expect(published).toHaveLength(1);
  });

  it('creates a simple-pool sender that requires one relay publish when configured', async () => {
    const signEvent = vi.fn(async (template: { kind: number; created_at: number; tags: string[][]; content: string }) => ({
      id: '1'.repeat(64),
      pubkey: 'signer',
      kind: template.kind,
      created_at: template.created_at,
      tags: template.tags,
      content: template.content,
      sig: '2'.repeat(128),
    } satisfies NostrEvent));
    const giftWrap = vi.fn();
    const publishAttempt = vi.fn(async () => undefined);
    const signalPool = {
      publish: vi.fn(() => [publishAttempt()]),
    };

    const sender = createSimplePoolSignalingSender({
      signEvent,
      giftWrap,
      publishMode: 'require-one',
      publishMaxWaitMs: 15_000,
    })({
      signalPool: signalPool as never,
      relayUrls: ['wss://relay.example'],
    });

    await sender({ type: 'hello', peerId: 'peer-a' });

    expect(signalPool.publish).toHaveBeenCalledTimes(1);
    expect(signalPool.publish).toHaveBeenCalledWith(
      ['wss://relay.example'],
      expect.objectContaining({
        tags: expect.arrayContaining([
          ['l', HELLO_TAG],
          ['peerId', 'peer-a'],
        ]),
      }),
    );
  });

  it('creates a simple-pool sender that best-effort publishes to all relays when configured', async () => {
    const directedEvent: NostrEvent = {
      id: '3'.repeat(64),
      pubkey: 'ephemeral',
      kind: SIGNALING_KIND,
      created_at: 1700000000,
      tags: [['p', 'recipient']],
      content: 'ciphertext',
      sig: '4'.repeat(128),
    };
    const signEvent = vi.fn();
    const giftWrap = vi.fn(async () => directedEvent);
    const signalPool = {
      publish: vi.fn(() => [
        Promise.resolve(undefined),
        Promise.reject(new Error('offline relay')),
      ]),
    };

    const sender = createSimplePoolSignalingSender({
      signEvent,
      giftWrap,
      publishMode: 'best-effort',
    })({
      signalPool: signalPool as never,
      relayUrls: ['wss://relay.one', 'wss://relay.two'],
    });

    await sender({
      type: 'offer',
      peerId: 'sender',
      targetPeerId: 'recipient',
      sdp: 'v=0',
    }, 'recipient');

    expect(giftWrap).toHaveBeenCalledTimes(1);
    expect(signalPool.publish).toHaveBeenCalledWith(
      ['wss://relay.one', 'wss://relay.two'],
      directedEvent,
    );
  });

  it('decodes hello events', async () => {
    const helloEvent: SignalingEventLike = {
      pubkey: 'sender-pubkey',
      created_at: Math.floor(Date.now() / 1000),
      tags: [
        ['l', HELLO_TAG],
        ['peerId', 'sender-pubkey'],
      ],
      content: '',
    };

    const decoded = await decodeSignalingEvent({
      event: helloEvent,
      giftUnwrap: async () => null,
      nowMs: () => Date.now(),
    });

    expect(decoded).toEqual({
      senderPubkey: 'sender-pubkey',
      message: {
        type: 'hello',
        peerId: 'sender-pubkey',
        hashGet: true,
      },
    });
  });

  it('decodes hello hashGet capability from tags', async () => {
    const helloEvent: SignalingEventLike = {
      pubkey: 'sender-pubkey',
      created_at: Math.floor(Date.now() / 1000),
      tags: [
        ['l', HELLO_TAG],
        ['peerId', 'sender-pubkey'],
        ['hashGet', '0'],
      ],
      content: '',
    };

    const decoded = await decodeSignalingEvent({
      event: helloEvent,
      giftUnwrap: async () => null,
      nowMs: () => Date.now(),
    });

    expect(decoded).toEqual({
      senderPubkey: 'sender-pubkey',
      message: {
        type: 'hello',
        peerId: 'sender-pubkey',
        hashGet: false,
      },
    });
  });

  it('decodes directed events from gift-unwrapped payload', async () => {
    const directedEvent: SignalingEventLike = {
      pubkey: 'ephemeral',
      created_at: Math.floor(Date.now() / 1000),
      tags: [['p', 'me']],
      content: 'ciphertext',
    };

    const seal: GiftSeal = {
      pubkey: 'sender-pubkey',
      kind: SIGNALING_KIND,
      tags: [],
      content: JSON.stringify({
        type: 'offer',
        peerId: 'sender-pubkey',
        recipient: 'target',
        offer: { sdp: 'v=0' },
      }),
    };

    const decoded = await decodeSignalingEvent({
      event: directedEvent,
      giftUnwrap: async () => seal,
      nowMs: () => Date.now(),
    });

    expect(decoded).toEqual({
      senderPubkey: 'sender-pubkey',
      message: {
        type: 'offer',
        peerId: 'sender-pubkey',
        targetPeerId: 'target',
        sdp: 'v=0',
      },
    });
  });

  it('ignores expired events', async () => {
    const nowMs = 1700000000000;
    const event: SignalingEventLike = {
      pubkey: 'sender',
      created_at: Math.floor((nowMs - 120000) / 1000),
      tags: [['l', HELLO_TAG], ['peerId', 'sender']],
      content: '',
    };

    const decoded = await decodeSignalingEvent({
      event,
      giftUnwrap: async () => null,
      nowMs: () => nowMs,
    });

    expect(decoded).toBeNull();
  });
});
