import { describe, expect, it, vi } from 'vitest';
import type { SignalingMessage } from '@hashtree/mesh';
import {
  finalizeEvent,
  generateSecretKey,
  getPublicKey,
  nip44,
  type Event as NostrEvent,
  verifyEvent,
} from 'nostr-tools';
import {
  SIGNALING_KIND,
  HELLO_TAG,
  createDecryptingGiftUnwrapper,
  createSecretKeyNip44GiftWrap,
  createSecretKeyEventSigner,
  createSecretKeyGiftUnwrapper,
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

  it('creates a secret-key event signer', async () => {
    const secretKey = generateSecretKey();
    const signEvent = createSecretKeyEventSigner(secretKey);
    const event = await signEvent({
      kind: SIGNALING_KIND,
      created_at: 1700000000,
      tags: [['l', HELLO_TAG]],
      content: '',
    });

    expect(event.pubkey).toBe(getPublicKey(secretKey));
    expect(event.sig).toHaveLength(128);
  });

  it('round-trips a signed NIP-59 seal inside the hashtree gift wrapper', async () => {
    const senderSecretKey = generateSecretKey();
    const recipientSecretKey = generateSecretKey();
    const recipientPubkey = getPublicKey(recipientSecretKey);
    const senderPubkey = getPublicKey(senderSecretKey);
    const giftWrap = createSecretKeyNip44GiftWrap(senderSecretKey, { nowMs: () => 1700000000000 });
    const unwrapGift = createSecretKeyGiftUnwrapper(recipientSecretKey);

    const wrapped = await giftWrap({
      kind: SIGNALING_KIND,
      content: JSON.stringify({
        type: 'offer',
        peerId: senderPubkey,
        targetPeerId: recipientPubkey,
        sdp: 'v=0',
      } satisfies SignalingMessage),
      tags: [],
    }, recipientPubkey);

    const outerConversationKey = nip44.v2.utils.getConversationKey(recipientSecretKey, wrapped.pubkey);
    const outer = JSON.parse(nip44.v2.decrypt(wrapped.content, outerConversationKey)) as {
      pubkey: string;
      content: string;
      seal: NostrEvent;
    };

    expect(outer.pubkey).toBe(senderPubkey);
    expect(outer.content).toContain('"targetPeerId"');
    expect(outer.seal.kind).toBe(13);
    expect(outer.seal.pubkey).toBe(senderPubkey);
    expect(verifyEvent(outer.seal)).toBe(true);

    const sealConversationKey = nip44.v2.utils.getConversationKey(recipientSecretKey, senderPubkey);
    const rumor = JSON.parse(nip44.v2.decrypt(outer.seal.content, sealConversationKey)) as Record<string, unknown>;
    expect(rumor.pubkey).toBe(senderPubkey);
    expect(rumor.sig).toBeUndefined();
    expect(rumor.content).toContain('"targetPeerId"');

    const unwrapped = await unwrapGift(wrapped);

    expect(unwrapped).toEqual({
      pubkey: senderPubkey,
      kind: SIGNALING_KIND,
      content: JSON.stringify({
        type: 'offer',
        peerId: senderPubkey,
        targetPeerId: recipientPubkey,
        sdp: 'v=0',
      }),
      tags: [],
    });
  });

  it('creates a decrypting gift unwrap helper from an injected decrypt function', async () => {
    const decrypt = vi.fn(async () => JSON.stringify({
      pubkey: 'sender',
      kind: SIGNALING_KIND,
      content: 'payload',
      tags: [['x', '1']],
    }));
    const unwrapGift = createDecryptingGiftUnwrapper(decrypt);
    const event: SignalingEventLike = {
      pubkey: 'ephemeral-pubkey',
      created_at: 1700000000,
      tags: [['p', 'recipient']],
      content: 'ciphertext',
    };

    const unwrapped = await unwrapGift(event);

    expect(decrypt).toHaveBeenCalledWith('ephemeral-pubkey', 'ciphertext');
    expect(unwrapped).toEqual({
      pubkey: 'sender',
      kind: SIGNALING_KIND,
      content: 'payload',
      tags: [['x', '1']],
    });
  });

  it('rejects a tampered signed seal instead of falling back to legacy wrapper content', async () => {
    const senderSecretKey = generateSecretKey();
    const signedSeal = finalizeEvent({
      kind: 13,
      created_at: 1700000000,
      tags: [],
      content: 'ciphertext',
    }, senderSecretKey);
    const decrypt = vi.fn(async () => JSON.stringify({
      pubkey: 'claimed-sender',
      kind: SIGNALING_KIND,
      content: JSON.stringify({
        type: 'offer',
        peerId: 'claimed-sender',
        targetPeerId: 'me',
        sdp: 'legacy-fallback',
      } satisfies SignalingMessage),
      tags: [],
      seal: {
        ...signedSeal,
        content: 'tampered',
      },
    }));
    const unwrapGift = createDecryptingGiftUnwrapper(decrypt);

    const unwrapped = await unwrapGift({
      pubkey: 'ephemeral',
      created_at: 1700000000,
      tags: [['p', 'me']],
      content: 'ciphertext',
    });

    expect(unwrapped).toBeNull();
    expect(decrypt).toHaveBeenCalledTimes(1);
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

  it('rejects signed events with invalid outer signatures', async () => {
    const secretKey = generateSecretKey();
    const pubkey = getPublicKey(secretKey);
    const event = finalizeEvent({
      kind: SIGNALING_KIND,
      created_at: Math.floor(Date.now() / 1000),
      tags: [
        ['l', HELLO_TAG],
        ['peerId', pubkey],
      ],
      content: '',
    }, secretKey);

    const decoded = await decodeSignalingEvent({
      event: {
        ...event,
        content: 'tampered',
      },
      giftUnwrap: async () => null,
      nowMs: () => Date.now(),
    });

    expect(decoded).toBeNull();
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
        targetPeerId: 'target',
        sdp: 'v=0',
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

  it('rejects legacy recipient-shaped directed payloads', async () => {
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
        recipient: 'me',
        offer: { sdp: 'v=0' },
      }),
    };

    const decoded = await decodeSignalingEvent({
      event: directedEvent,
      giftUnwrap: async () => seal,
      nowMs: () => Date.now(),
    });

    expect(decoded).toBeNull();
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
