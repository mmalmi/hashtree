import { execFileSync } from 'node:child_process';
import path from 'node:path';
import { describe, expect, it } from 'vitest';
import type { SignalingMessage } from '@hashtree/mesh';
import { getPublicKey } from 'nostr-tools';
import {
  createSecretKeyGiftUnwrapper,
  createSecretKeyNip44GiftWrap,
  decodeSignalingEvent,
  SIGNALING_KIND,
} from '../src/p2p/signaling.js';

const senderSecretHex = '0101010101010101010101010101010101010101010101010101010101010101';
const recipientSecretHex = '0202020202020202020202020202020202020202020202020202020202020202';
const sdp = 'v=0\r\ns=hashtree-signaling-interop\r\n';

function repoRoot(): string {
  return path.resolve(__dirname, '../../../..');
}

function cargoRoot(): string {
  return path.join(repoRoot(), 'rust');
}

function fromHex(hex: string): Uint8Array {
  const bytes = new Uint8Array(hex.length / 2);
  for (let i = 0; i < bytes.length; i += 1) {
    bytes[i] = Number.parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  }
  return bytes;
}

function runFixture(args: string[], input?: string): string {
  return execFileSync(
    'cargo',
    ['run', '-q', '-p', 'hashtree-network', '--bin', 'signaling_fixture', '--', ...args],
    {
      cwd: cargoRoot(),
      input,
      encoding: 'utf8',
      stdio: input === undefined ? ['ignore', 'pipe', 'inherit'] : ['pipe', 'pipe', 'inherit'],
    },
  ).trim();
}

describe('Rust signaling interop', () => {
  it('decodes a Rust-authenticated directed signaling event in TypeScript', async () => {
    const recipientSecretKey = fromHex(recipientSecretHex);
    const senderPubkey = getPublicKey(fromHex(senderSecretHex));
    const recipientPubkey = getPublicKey(recipientSecretKey);
    const rustEvent = JSON.parse(runFixture(['encode-offer']));

    const decoded = await decodeSignalingEvent({
      event: rustEvent,
      giftUnwrap: createSecretKeyGiftUnwrapper(recipientSecretKey),
      nowMs: () => Date.now(),
      maxEventAgeSec: Number.POSITIVE_INFINITY,
    });

    expect(decoded).toEqual({
      senderPubkey,
      message: {
        type: 'offer',
        peerId: senderPubkey,
        targetPeerId: recipientPubkey,
        sdp,
      },
    });
  }, 120000);

  it('decodes a TypeScript-authenticated directed signaling event in Rust', async () => {
    const senderSecretKey = fromHex(senderSecretHex);
    const recipientPubkey = getPublicKey(fromHex(recipientSecretHex));
    const senderPubkey = getPublicKey(senderSecretKey);
    const giftWrap = createSecretKeyNip44GiftWrap(senderSecretKey);
    const event = await giftWrap({
      kind: SIGNALING_KIND,
      tags: [],
      content: JSON.stringify({
        type: 'offer',
        peerId: senderPubkey,
        targetPeerId: recipientPubkey,
        sdp,
      } satisfies SignalingMessage),
    }, recipientPubkey);

    const decoded = JSON.parse(runFixture(['decode-offer'], JSON.stringify(event))) as SignalingMessage;

    expect(decoded).toEqual({
      type: 'offer',
      peerId: senderPubkey,
      targetPeerId: recipientPubkey,
      sdp,
    });
  }, 120000);
});
