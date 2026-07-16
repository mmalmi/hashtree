import { type FipsDatagramEndpoint } from '@fips/tcp';
import { MemoryStore, sha256, type Hash, type Store } from '@hashtree/core';
import { describe, expect, it, vi } from 'vitest';
import { TcpBlobTransport } from '../src/tcpBlobTransport.js';

type FetchFromPeer = (
  peer: string,
  hash: Hash,
  timeoutMs: number,
  htl: number,
) => Promise<Uint8Array | null>;

class IdleEndpoint implements FipsDatagramEndpoint {
  registerService(): () => void {
    return () => undefined;
  }

  async sendDatagram(): Promise<void> {
    throw new Error('unexpected TCP/FIPS datagram');
  }
}

describe('TCP/FIPS blob provider result semantics', () => {
  it('reports unavailable when there are no providers', async () => {
    await withTransport(async (transport, fetch) => {
      await expect(transport.get(hash(), ['', '  '])).rejects.toThrow(
        'no TCP/FIPS blob providers are available',
      );
      expect(fetch).not.toHaveBeenCalled();
    });
  });

  it('returns missing only when every attempted provider explicitly reports missing', async () => {
    await withTransport(async (transport, fetch) => {
      fetch.mockResolvedValue(null);

      await expect(transport.get(hash(), ['peer-a', 'peer-b', 'peer-a'])).resolves.toBeNull();
      expect(fetch.mock.calls.map(([peer]) => peer)).toEqual(['peer-a', 'peer-b']);
      expect(fetch.mock.calls.map(([, , , htl]) => htl)).toEqual([10, 10]);
    });
  });

  it('preserves an explicit local-only HTL', async () => {
    await withTransport(async (transport, fetch) => {
      fetch.mockResolvedValue(null);

      await expect(transport.get(hash(), ['peer-a'], 0)).resolves.toBeNull();
      expect(fetch.mock.calls.map(([, , , htl]) => htl)).toEqual([0]);
    });
  });

  it('forwards the route HTL unchanged to every provider attempt', async () => {
    await withTransport(async (transport, fetch) => {
      fetch.mockResolvedValue(null);

      await expect(transport.get(hash(), ['peer-a', 'peer-b'], 3)).resolves.toBeNull();
      expect(fetch.mock.calls.map(([peer, , , htl]) => [peer, htl])).toEqual([
        ['peer-a', 3],
        ['peer-b', 3],
      ]);
    });
  });

  it('reports unavailable when every provider fails', async () => {
    await withTransport(async (transport, fetch) => {
      fetch.mockRejectedValue(new Error('provider failed'));

      await expect(transport.get(hash(), ['peer-a', 'peer-b'])).rejects.toThrow(
        'TCP/FIPS blob availability is uncertain',
      );
      expect(fetch).toHaveBeenCalledTimes(4);
    });
  });

  it('does not turn a failure and explicit miss into false absence', async () => {
    await withTransport(async (transport, fetch) => {
      fetch.mockImplementation(async (peer) => {
        if (peer === 'peer-a') return null;
        throw new Error('peer-b disconnected');
      });

      await expect(transport.get(hash(), ['peer-a', 'peer-b'])).rejects.toThrow(
        'TCP/FIPS blob availability is uncertain',
      );
    });
  });

  it('lets a later unanimous miss resolve an earlier provider failure', async () => {
    await withTransport(async (transport, fetch) => {
      fetch.mockImplementation(async () => {
        if (fetch.mock.calls.length <= 2) throw new Error('first session failed');
        return null;
      });

      await expect(transport.get(hash(), ['peer-a', 'peer-b'])).resolves.toBeNull();
      expect(fetch).toHaveBeenCalledTimes(4);
    });
  });

  it('returns a valid fallback result after another provider fails', async () => {
    await withTransport(async (transport, fetch) => {
      const expected = new TextEncoder().encode('fallback data');
      fetch.mockImplementation(async (peer) => {
        const peerCalls = fetch.mock.calls.filter(([calledPeer]) => calledPeer === peer).length;
        if (peer === 'peer-b' && peerCalls === 2) return expected;
        if (peer === 'peer-a') return null;
        throw new Error('peer-b first session failed');
      });

      await expect(transport.get(hash(), ['peer-a', 'peer-b'])).resolves.toEqual(expected);
    });
  });

  it('returns the first valid provider result without waiting for a stalled route', async () => {
    await withTransport(async (transport, fetch) => {
      const expected = new TextEncoder().encode('first verified data');
      let releaseStalled!: (value: Uint8Array | null) => void;
      const stalled = new Promise<Uint8Array | null>((resolve) => {
        releaseStalled = resolve;
      });
      fetch.mockImplementation((peer) => peer === 'peer-a' ? stalled : Promise.resolve(expected));

      const result = transport.get(hash(), ['peer-a', 'peer-b']);
      try {
        await expect(Promise.race([
          result,
          new Promise((resolve) => setTimeout(() => resolve('stalled'), 50)),
        ])).resolves.toEqual(expected);
      } finally {
        releaseStalled(null);
        await result.catch(() => undefined);
      }
    });
  });

  it('reports corrupt local bytes as an integrity error, never a miss', async () => {
    const expected = new TextEncoder().encode('expected');
    const expectedHash = await sha256(expected) as Hash;
    const corruptStore: Store = {
      get: async () => new TextEncoder().encode('corrupt'),
      put: async () => true,
      has: async () => true,
      delete: async () => true,
    };
    const transport = new TcpBlobTransport({
      endpoint: new IdleEndpoint(),
      localStore: corruptStore,
      timeoutMs: 1_000,
    });
    try {
      await expect(transport.get(expectedHash, [])).rejects.toThrow('local blob hash mismatch');
    } finally {
      await transport.close();
    }
  });
});

async function withTransport(
  run: (
    transport: TcpBlobTransport,
    fetch: ReturnType<typeof vi.fn<FetchFromPeer>>,
  ) => Promise<void>,
): Promise<void> {
  const transport = new TcpBlobTransport({
    endpoint: new IdleEndpoint(),
    localStore: new MemoryStore(),
    timeoutMs: 1_000,
  });
  const fetch = vi.fn<FetchFromPeer>();
  (transport as unknown as { fetchFromPeer: FetchFromPeer }).fetchFromPeer = fetch;
  try {
    await run(transport, fetch);
  } finally {
    await transport.close();
  }
}

function hash(): Hash {
  return new Uint8Array(32);
}
