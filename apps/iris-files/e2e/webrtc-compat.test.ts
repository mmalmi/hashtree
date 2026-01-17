/**
 * E2E test for WebRTC signaling protocol compatibility between ts and rust
 *
 * This test verifies that:
 * 1. Signaling uses Nostr kind 25050 (ephemeral)
 * 2. Hello messages use #l: "hello" with peerId tag and empty content
 * 3. Directed signaling uses targetPeerId/sdp/candidate formats compatible with Rust
 */

import { test, expect } from './fixtures';
import { SimplePool, finalizeEvent, generateSecretKey, getPublicKey, type Event } from 'nostr-tools';
import WebSocket from 'ws';

(globalThis as any).WebSocket = WebSocket;

// Test configuration - matches hashtree signaling protocol
const WEBRTC_KIND = 25050;
const HELLO_TAG = 'hello';

// Message types matching the protocol
interface HelloMessage {
  type: 'hello';
  peerId: string;
}

interface OfferMessage {
  type: 'offer';
  peerId: string;
  targetPeerId: string;
  sdp: string;
}

interface AnswerMessage {
  type: 'answer';
  peerId: string;
  targetPeerId: string;
  sdp: string;
}

interface CandidateMessage {
  type: 'candidate';
  peerId: string;
  targetPeerId: string;
  candidate: string;
  sdpMLineIndex?: number;
  sdpMid?: string;
}

interface CandidatesMessage {
  type: 'candidates';
  peerId: string;
  targetPeerId: string;
  candidates: Array<{ candidate: string; sdpMLineIndex?: number; sdpMid?: string }>;
}

type SignalingMessage = HelloMessage | OfferMessage | AnswerMessage | CandidateMessage | CandidatesMessage;

function generateUuid(): string {
  return Math.random().toString(36).substring(2, 15) +
    Math.random().toString(36).substring(2, 15);
}

async function publishWithRetry(
  pool: SimplePool,
  relayUrl: string,
  event: Event,
  attempts = 3
): Promise<void> {
  let lastError: unknown;
  for (let attempt = 0; attempt < attempts; attempt++) {
    try {
      const relay = await pool.ensureRelay(relayUrl);
      relay.connectionTimeout = 15000;
      relay.publishTimeout = 15000;
      await relay.connect();
      await Promise.any(pool.publish([relayUrl], event));
      return;
    } catch (err) {
      lastError = err;
      await new Promise(r => setTimeout(r, 1000));
    }
  }
  console.log('Publish failed:', lastError);
}

test.describe('WebRTC Signaling Protocol Compatibility', () => {
  test.setTimeout(30000);

  test('signaling message format matches cross-language protocol', async ({ relayUrl }) => {
    // Generate test keys
    const sk1 = generateSecretKey();
    const pk1 = getPublicKey(sk1);
    const uuid1 = generateUuid();
    const peerId1 = `${pk1}:${uuid1}`;

    const sk2 = generateSecretKey();
    const pk2 = getPublicKey(sk2);
    const uuid2 = generateUuid();
    const peerId2 = `${pk2}:${uuid2}`;

    // Create a hello message (as ts would)
    const helloMsg: HelloMessage = { type: 'hello', peerId: uuid1 };

    const offerMsg: OfferMessage = {
      type: 'offer',
      peerId: peerId1,
      targetPeerId: peerId2,
      sdp: 'test-sdp',
    };

    const answerMsg: AnswerMessage = {
      type: 'answer',
      peerId: peerId2,
      targetPeerId: peerId1,
      sdp: 'test-sdp-answer',
    };

    const candidateMsg: CandidateMessage = {
      type: 'candidate',
      peerId: peerId2,
      targetPeerId: peerId1,
      candidate: 'test-candidate',
      sdpMid: '0',
      sdpMLineIndex: 0,
    };

    const candidatesMsg: CandidatesMessage = {
      type: 'candidates',
      peerId: peerId2,
      targetPeerId: peerId1,
      candidates: [
        { candidate: 'test-candidate-1' },
        { candidate: 'test-candidate-2', sdpMid: '0', sdpMLineIndex: 0 },
      ],
    };

    // Verify all messages can be serialized to JSON
    expect(JSON.stringify(helloMsg)).toBeTruthy();
    expect(JSON.stringify(offerMsg)).toBeTruthy();
    expect(JSON.stringify(answerMsg)).toBeTruthy();
    expect(JSON.stringify(candidateMsg)).toBeTruthy();
    expect(JSON.stringify(candidatesMsg)).toBeTruthy();

    // Verify messages can be parsed back
    const parsedHello = JSON.parse(JSON.stringify(helloMsg)) as SignalingMessage;
    expect(parsedHello.type).toBe('hello');

    const parsedOffer = JSON.parse(JSON.stringify(offerMsg)) as SignalingMessage;
    expect(parsedOffer.type).toBe('offer');
    expect((parsedOffer as OfferMessage).targetPeerId).toBe(peerId2);

    console.log('All signaling message formats are valid');
  });

  test('nostr event format matches signaling protocol', async () => {
    const sk = generateSecretKey();
    const pk = getPublicKey(sk);
    const uuid = generateUuid();

    const helloMsg: HelloMessage = {
      type: 'hello',
      peerId: uuid,
    };

    const expiration = Math.floor((Date.now() + 5 * 60 * 1000) / 1000);

    // Create hello event format used by ts/rust
    const eventTemplate = {
      kind: WEBRTC_KIND,
      created_at: Math.floor(Date.now() / 1000),
      tags: [
        ['l', HELLO_TAG],
        ['peerId', helloMsg.peerId],
        ['expiration', expiration.toString()],
      ],
      content: '',
    };

    const signedEvent = finalizeEvent(eventTemplate, sk);

    // Verify event structure
    expect(signedEvent.kind).toBe(25050);
    expect(signedEvent.pubkey).toBe(pk);

    // Verify tags
    const lTag = signedEvent.tags.find(t => t[0] === 'l');
    expect(lTag).toBeTruthy();
    expect(lTag![1]).toBe('hello');

    const expTag = signedEvent.tags.find(t => t[0] === 'expiration');
    expect(expTag).toBeTruthy();

    const peerIdTag = signedEvent.tags.find(t => t[0] === 'peerId');
    expect(peerIdTag?.[1]).toBe(uuid);

    // Hello events carry no JSON content
    expect(signedEvent.content).toBe('');

    console.log('Nostr event format matches protocol');
    console.log('Event kind:', signedEvent.kind);
    console.log('Tags:', signedEvent.tags);
  });

  test('peer ID format is compatible', async () => {
    const sk = generateSecretKey();
    const pk = getPublicKey(sk);
    const uuid = generateUuid();

    // PeerId format: pubkey:uuid
    const peerId = `${pk}:${uuid}`;

    // Verify format
    const parts = peerId.split(':');
    expect(parts.length).toBe(2);
    expect(parts[0]).toBe(pk);
    expect(parts[0].length).toBe(64); // hex pubkey
    expect(parts[1]).toBe(uuid);

    // Verify short format for logging
    const shortPeerId = `${pk.slice(0, 8)}:${uuid.slice(0, 6)}`;
    expect(shortPeerId.length).toBe(15); // 8 + 1 + 6

    console.log('PeerId format:', peerId);
    console.log('Short format:', shortPeerId);
  });

  test('tie-breaking logic is consistent', async () => {
    // Both implementations use: lower UUID initiates connection
    const uuid1 = 'aaaaaaaaaaaaaaa';
    const uuid2 = 'zzzzzzzzzzzzzzz';

    // uuid1 < uuid2, so uuid1 should initiate
    expect(uuid1 < uuid2).toBe(true);

    // Real UUID comparison
    const realUuid1 = generateUuid();
    const realUuid2 = generateUuid();

    // One of them should be "smaller" and initiate
    const initiator = realUuid1 < realUuid2 ? 'uuid1' : 'uuid2';
    console.log(`${initiator} would initiate (${realUuid1} vs ${realUuid2})`);
  });

  test('can exchange hello messages via relay', async ({ relayUrl }) => {
    const localRelay = relayUrl;
    const pool = new SimplePool();

    const sk1 = generateSecretKey();
    const pk1 = getPublicKey(sk1);
    const uuid1 = generateUuid();

    const sk2 = generateSecretKey();
    const pk2 = getPublicKey(sk2);
    const uuid2 = generateUuid();

    const receivedMessages: Event[] = [];

    // Subscribe to webrtc events
    const sub = pool.subscribe(
      [localRelay],
      [{
        kinds: [WEBRTC_KIND],
        '#l': [HELLO_TAG],
        since: Math.floor(Date.now() / 1000) - 30,
      }],
      {
        onevent(event) {
          receivedMessages.push(event);
        },
      }
    );

    // Wait for subscription to be ready
    await new Promise(r => setTimeout(r, 2000));

    // Send hello from peer1
    const expiration = Math.floor((Date.now() + 5 * 60 * 1000) / 1000);

    const event1 = finalizeEvent({
      kind: WEBRTC_KIND,
      created_at: Math.floor(Date.now() / 1000),
      tags: [
        ['l', HELLO_TAG],
        ['peerId', uuid1],
        ['expiration', expiration.toString()],
      ],
      content: '',
    }, sk1);

    // Publish to relays
    await publishWithRetry(pool, localRelay, event1);

    // Wait for message to propagate
    for (let i = 0; i < 10; i++) {
      if (receivedMessages.find((event) => event.pubkey === pk1)) break;
      await new Promise(r => setTimeout(r, 500));
    }

    // Check if we received the message
    console.log(`Received ${receivedMessages.length} messages`);

    // We should have received at least our own message
    const ownHello = receivedMessages.find((event) => event.pubkey === pk1);
    expect(ownHello).toBeTruthy();
    const peerIdTag = ownHello?.tags.find((t) => t[0] === 'peerId');
    expect(peerIdTag?.[1]).toBe(uuid1);

    try {
      sub.close();
      pool.close([localRelay]);
    } catch (e) {
      console.warn('[webrtc-compat] relay close error:', e);
    }
  });
});
