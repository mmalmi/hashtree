import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { MemoryStore, fromHex, toHex, type CID } from '@hashtree/core';

const decodeMock = vi.hoisted(() => vi.fn());
const socketPlanMock = vi.hoisted(() => vi.fn());
const socketSendMock = vi.hoisted(() => vi.fn());
const socketCloseMock = vi.hoisted(() => vi.fn());

vi.mock('nostr-tools', () => ({
  nip19: {
    decode: (...args: Parameters<typeof decodeMock>) => decodeMock(...args),
  },
}));

import {
  resolveRootPathFromRelays,
  watchRootPathFromRelays,
} from '../src/capabilities/rootResolver.js';
import { clearMemoryCache, initTreeRootCache, setCachedRoot } from '../src/relay/treeRootCache.js';

const NPUB = 'npub1g53mukxnjkcmr94fhryzkqutdz2ukq4ks0gvy5af25rgmwsl4ngq43drvk';
const PUBKEY = '1'.repeat(64);
const ROOT_HASH = '2'.repeat(64);
const EXACT_HASH = '3'.repeat(64);
const CHILD: CID = { hash: Uint8Array.from({ length: 32 }, (_, index) => index + 1) };

class FakeWebSocket {
  static readonly OPEN = 1;

  readonly url: string;
  readyState = FakeWebSocket.OPEN;
  onopen: ((event: Event) => void) | null = null;
  onmessage: ((event: MessageEvent) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;

  constructor(url: string) {
    this.url = url;
    queueMicrotask(() => {
      this.onopen?.(new Event('open'));
      socketPlanMock(this, url);
    });
  }

  send(data: string): void {
    socketSendMock(this.url, data);
  }

  close(): void {
    socketCloseMock(this.url);
  }

  emitMessage(message: unknown[]): void {
    this.onmessage?.({ data: JSON.stringify(message) } as MessageEvent);
  }
}

function makeEvent(treeName: string, hash: string, createdAt = 1_700_000_000) {
  return {
    id: `${hash}${hash}`.slice(0, 64),
    pubkey: PUBKEY,
    kind: 30078,
    content: '',
    tags: [
      ['d', treeName],
      ['l', 'hashtree'],
      ['hash', hash],
    ],
    created_at: createdAt,
    sig: '4'.repeat(128),
  };
}

describe('rootResolver capability', () => {
  beforeEach(() => {
    decodeMock.mockReset();
    socketPlanMock.mockReset();
    socketSendMock.mockReset();
    socketCloseMock.mockReset();
    clearMemoryCache();
    initTreeRootCache(new MemoryStore());
    Object.defineProperty(globalThis, 'WebSocket', {
      configurable: true,
      writable: true,
      value: FakeWebSocket,
    });
  });

  afterEach(() => {
    vi.useRealTimers();
    // @ts-expect-error test cleanup
    delete globalThis.WebSocket;
  });

  it('returns an exact tree match without resolving a subpath', async () => {
    decodeMock.mockReturnValue({ type: 'npub', data: PUBKEY });
    socketPlanMock.mockImplementation((socket: FakeWebSocket, url: string) => {
      if (url === 'wss://relay.example') {
        const requestMessages = socketSendMock.mock.calls
          .filter(([relayUrl]) => relayUrl === 'wss://relay.example')
          .map(([, data]) => JSON.parse(data as string));
        const request = requestMessages[requestMessages.length - 1];
        socket.emitMessage(['EVENT', request[1], makeEvent('audio-catalog/root.json', EXACT_HASH)]);
      }
    });

    const resolvePath = vi.fn();
    const resolved = await resolveRootPathFromRelays({ resolvePath }, ['wss://relay.example'], NPUB, 'audio-catalog/root.json');

    expect(toHex(resolved!.hash)).toBe(EXACT_HASH);
    expect(resolvePath).not.toHaveBeenCalled();
    expect(socketSendMock).toHaveBeenCalled();
  });

  it('returns a cached exact tree match without opening relay subscriptions', async () => {
    decodeMock.mockReturnValue({ type: 'npub', data: PUBKEY });
    await setCachedRoot(NPUB, 'audio-catalog/root.json', { hash: fromHex(EXACT_HASH) });

    const resolvePath = vi.fn();
    const resolved = await resolveRootPathFromRelays(
      { resolvePath },
      ['wss://relay.example'],
      NPUB,
      'audio-catalog/root.json',
    );

    expect(toHex(resolved!.hash)).toBe(EXACT_HASH);
    expect(resolvePath).not.toHaveBeenCalled();
    expect(socketSendMock).not.toHaveBeenCalled();
  });

  it('falls back to the tree root and resolves the remaining subpath', async () => {
    decodeMock.mockReturnValue({ type: 'npub', data: PUBKEY });
    socketPlanMock.mockImplementation((socket: FakeWebSocket, url: string) => {
      if (url === 'wss://relay.example') {
        const requestMessages = socketSendMock.mock.calls
          .filter(([relayUrl]) => relayUrl === 'wss://relay.example')
          .map(([, data]) => JSON.parse(data as string));
        const request = requestMessages[requestMessages.length - 1];
        const requestedTreeName = request?.[2]?.['#d']?.[0];
        if (requestedTreeName === 'audio-catalog') {
          socket.emitMessage(['EVENT', request[1], makeEvent('audio-catalog', ROOT_HASH)]);
        }
      }
    });

    const resolvePath = vi.fn().mockResolvedValue({ cid: CHILD });
    const resolved = await resolveRootPathFromRelays(
      { resolvePath },
      ['wss://relay.example'],
      NPUB,
      'audio-catalog/root.json',
      1_234,
    );

    expect(resolved).toEqual(CHILD);
    expect(resolvePath).toHaveBeenCalledTimes(1);
    expect(resolvePath).toHaveBeenCalledWith(
      expect.objectContaining({ hash: expect.any(Uint8Array) }),
      ['root.json'],
    );
    expect(toHex((resolvePath.mock.calls[0]![0] as CID).hash)).toBe(ROOT_HASH);
    expect(socketSendMock).toHaveBeenCalled();
  });

  it('waits for a newer event that arrives before the query window closes', async () => {
    decodeMock.mockReturnValue({ type: 'npub', data: PUBKEY });
    socketPlanMock.mockImplementation((socket: FakeWebSocket, url: string) => {
      if (url !== 'wss://relay.example') {
        return;
      }
      const requestMessages = socketSendMock.mock.calls
        .filter(([relayUrl]) => relayUrl === 'wss://relay.example')
        .map(([, data]) => JSON.parse(data as string));
      const request = requestMessages[requestMessages.length - 1];
      socket.emitMessage(['EVENT', request[1], makeEvent('audio-catalog/root.json', '5'.repeat(64), 100)]);
      setTimeout(() => {
        socket.emitMessage(['EVENT', request[1], makeEvent('audio-catalog/root.json', '6'.repeat(64), 200)]);
      }, 20);
    });

    const resolvePromise = resolveRootPathFromRelays(
      { resolvePath: vi.fn() },
      ['wss://relay.example'],
      NPUB,
      'audio-catalog/root.json',
      200,
      50,
    );

    const resolved = await resolvePromise;

    expect(toHex(resolved!.hash)).toBe('6'.repeat(64));
    expect(socketSendMock).toHaveBeenCalled();
  });

  it('returns a cached initial root immediately for live watches', async () => {
    decodeMock.mockReturnValue({ type: 'npub', data: PUBKEY });
    await setCachedRoot(NPUB, 'audio-catalog/root.json', { hash: fromHex(EXACT_HASH) });

    const onUpdate = vi.fn();
    const watch = await watchRootPathFromRelays(
      { resolvePath: vi.fn() },
      ['wss://relay.example'],
      NPUB,
      'audio-catalog/root.json',
      onUpdate,
    );

    expect(toHex(watch.initialCid!.hash)).toBe(EXACT_HASH);
    await Promise.resolve();
    expect(socketSendMock).toHaveBeenCalled();
    expect(onUpdate).not.toHaveBeenCalled();

    await watch.close();
  });
});
