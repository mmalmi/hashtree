import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { MemoryStore } from '@hashtree/core';
import type { ConnectionId, FipsDatagramEndpoint } from '@fips/tcp';
import { TcpBlobTransport } from '../src/tcpBlobTransport.js';

type WritePath = {
  writeAll(connection: ConnectionId, data: Uint8Array, deadline: number, response?: boolean): Promise<void>;
  tcp: { write(connection: ConnectionId, data: Uint8Array): Promise<number> };
};
const transports: TcpBlobTransport[] = [];
const connection = 1 as ConnectionId;

function writer(limit: number | null) {
  const endpoint: FipsDatagramEndpoint = {
    registerService: () => () => {},
    sendDatagram: async () => { throw new Error('unexpected datagram'); },
  };
  const transport = new TcpBlobTransport({
    endpoint, localStore: new MemoryStore(), getUploadLimitBytesPerSecond: () => limit,
  });
  transports.push(transport);
  const path = transport as unknown as WritePath;
  const write = vi.spyOn(path.tcp, 'write').mockImplementation(async (_connection, data) => data.byteLength);
  return { transport, path, write };
}

beforeEach(() => { vi.useFakeTimers(); vi.setSystemTime(0); });
afterEach(async () => {
  for (const transport of transports.splice(0)) await transport.close();
  vi.useRealTimers();
});

describe('TCP/FIPS inbound response upload limit', () => {
  it('writes a response larger than its one-second bucket in bounded chunks', async () => {
    const { path, write } = writer(4);
    const pending = path.writeAll(connection, new Uint8Array(10), 2_000, true);
    void pending.catch(() => {});
    await vi.advanceTimersByTimeAsync(0);
    expect(write.mock.calls.map(([, bytes]) => bytes.byteLength)).toEqual([4]);
    await vi.advanceTimersByTimeAsync(1_000);
    expect(write.mock.calls.map(([, bytes]) => bytes.byteLength)).toEqual([4, 4]);
    await vi.advanceTimersByTimeAsync(500);
    await expect(pending).resolves.toBeUndefined();
    expect(write.mock.calls.map(([, bytes]) => bytes.byteLength)).toEqual([4, 4, 2]);
  });

  it('refunds bytes the TCP queue did not accept', async () => {
    const { path, write } = writer(4);
    write.mockResolvedValueOnce(2);
    const pending = path.writeAll(connection, new Uint8Array(4), 100, true);
    void pending.catch(() => {});
    await vi.advanceTimersByTimeAsync(0);
    expect(write.mock.calls.map(([, bytes]) => bytes.byteLength)).toEqual([4, 2]);
    await expect(pending).resolves.toBeUndefined();
  });

  it.each([0, null])('keeps %s unlimited', async (limit) => {
    const { path, write } = writer(limit);
    await path.writeAll(connection, new Uint8Array(10), 100, true);
    expect(write).toHaveBeenCalledOnce();
    expect(write.mock.calls[0][1]).toHaveLength(10);
  });

  it('does not throttle outgoing request writes', async () => {
    const { path, write } = writer(1);
    await path.writeAll(connection, new Uint8Array(36), 100);
    expect(write.mock.calls[0][1]).toHaveLength(36);
  });

  it('bounds a full bucket wait by the original response deadline', async () => {
    const { path, write } = writer(1);
    const pending = path.writeAll(connection, new Uint8Array(2), 150, true);
    const failure = expect(pending).rejects.toThrow('timed out');
    await vi.advanceTimersByTimeAsync(150);
    await failure;
    expect(write.mock.calls.map(([, bytes]) => bytes.byteLength)).toEqual([1]);
  });

  it('settles a waiting response when the transport closes', async () => {
    const { transport, path, write } = writer(1);
    const pending = path.writeAll(connection, new Uint8Array(2), 20_000, true);
    const failure = expect(pending).rejects.toThrow('closed');
    await vi.advanceTimersByTimeAsync(0);
    await transport.close();
    await vi.advanceTimersByTimeAsync(10);
    await failure;
    expect(write.mock.calls.map(([, bytes]) => bytes.byteLength)).toEqual([1]);
  });

  it('shares the bucket across responses and closes every waiting writer', async () => {
    const { transport, path, write } = writer(4);
    const writes = [1, 2, 3].map((id) => path.writeAll(id as ConnectionId, new Uint8Array(4), 20_000, true));
    const settled = Promise.allSettled(writes);
    await vi.advanceTimersByTimeAsync(0);
    expect(write).toHaveBeenCalledOnce();
    await transport.close();
    await vi.advanceTimersByTimeAsync(10);
    expect((await settled).map((result) => result.status)).toEqual(['fulfilled', 'rejected', 'rejected']);
    expect(write).toHaveBeenCalledOnce();
  });

});
