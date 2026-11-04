import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { FipsTcpEndpoint, type ConnectionId } from '@fips/tcp';
import { MemoryStore, sha256 } from '@hashtree/core';
import { TcpBlobTransport } from '../src/tcpBlobTransport.js';

type Io = {
  tcp: FipsTcpEndpoint;
  writeAll(connection: ConnectionId, data: Uint8Array, deadline: number): Promise<void>;
  readExact(connection: ConnectionId, size: number, deadline: number): Promise<Uint8Array>;
};

const transports: TcpBlobTransport[] = [];
const hash = new Uint8Array(32);
const connection = 1 as ConnectionId;

function fixture(store = new MemoryStore()) {
  const send = vi.fn(async () => {});
  const transport = new TcpBlobTransport({
    endpoint: { registerService: () => () => {}, sendDatagram: send },
    localStore: store,
    timeoutMs: 5_500,
  });
  transports.push(transport);
  return { transport, io: transport as unknown as Io, send };
}

function observe(pending: Promise<unknown>) {
  let result: unknown = 'pending';
  void pending.then((value) => { result = value; }, (error) => { result = error; });
  return () => result;
}

beforeEach(() => { vi.useFakeTimers(); });
afterEach(async () => {
  await Promise.all(transports.splice(0).map((transport) => transport.close()));
  vi.restoreAllMocks();
  vi.useRealTimers();
});

describe('TCP/FIPS transport cancellation', () => {
  it('rejects a stalled real TCP handshake immediately and never retries after close', async () => {
    const { transport, io, send } = fixture();
    const connect = vi.spyOn(io.tcp, 'connect');
    const abort = vi.spyOn(io.tcp, 'abort');
    const result = observe(transport.get(hash, ['silent-peer']));
    await vi.advanceTimersByTimeAsync(0);
    expect(send).toHaveBeenCalled();
    expect(connect).toHaveBeenCalledOnce();
    await transport.close();
    await vi.advanceTimersByTimeAsync(0);
    expect(result()).toEqual(new Error('TCP/FIPS transport is closed'));
    expect(abort).toHaveBeenCalledOnce();
    await vi.advanceTimersByTimeAsync(6_000);
    expect(connect).toHaveBeenCalledOnce();
  });

  it('rejects new reads even for cached data and does not probe after close', async () => {
    const store = new MemoryStore();
    const bytes = new Uint8Array([1, 2, 3]);
    const cachedHash = await sha256(bytes);
    await store.put(cachedHash, bytes);
    const { transport, send } = fixture(store);
    await transport.close();
    await expect(transport.get(cachedHash, ['peer'])).rejects.toThrow('closed');
    await expect(transport.probe('peer')).resolves.toBe(false);
    expect(send).not.toHaveBeenCalled();
  });

  it('cancels a held local lookup without starting a later network request', async () => {
    const store = new MemoryStore();
    let release!: (data: Uint8Array | null) => void;
    vi.spyOn(store, 'get').mockReturnValue(new Promise((resolve) => { release = resolve; }));
    const { transport, send } = fixture(store);
    const result = observe(transport.get(hash, ['peer']));
    await transport.close();
    await vi.advanceTimersByTimeAsync(0);
    expect(result()).toEqual(new Error('TCP/FIPS transport is closed'));
    release(null);
    await vi.advanceTimersByTimeAsync(6_000);
    expect(send).not.toHaveBeenCalled();
  });

  it.each(['write', 'read'] as const)('aborts a backpressured outbound %s without another poll', async (operation) => {
    const { transport, io } = fixture();
    const write = vi.spyOn(io.tcp, 'write').mockResolvedValue(0);
    const read = vi.spyOn(io.tcp, 'read').mockResolvedValue(new Uint8Array());
    vi.spyOn(io.tcp, 'isReadClosed').mockResolvedValue(false);
    const pending = operation === 'write'
      ? io.writeAll(connection, new Uint8Array(36), Date.now() + 10_000)
      : io.readExact(connection, 7, Date.now() + 10_000);
    const result = observe(pending);
    await vi.advanceTimersByTimeAsync(0);
    const calls = operation === 'write' ? write : read;
    expect(calls).toHaveBeenCalledOnce();
    await transport.close();
    await vi.advanceTimersByTimeAsync(0);
    expect(result()).toEqual(new Error('TCP/FIPS transport is closed'));
    await vi.advanceTimersByTimeAsync(10_000);
    expect(calls).toHaveBeenCalledOnce();
  });

  it('aborts a late connection without sending a request or starting another session', async () => {
    const { transport, io } = fixture();
    let release!: (id: ConnectionId) => void;
    const connect = vi.spyOn(io.tcp, 'connect').mockReturnValue(new Promise((resolve) => { release = resolve; }));
    const abort = vi.spyOn(io.tcp, 'abort').mockResolvedValue();
    const write = vi.spyOn(io.tcp, 'write');
    const result = observe(transport.get(hash, ['peer']));
    await vi.advanceTimersByTimeAsync(0);
    expect(connect).toHaveBeenCalledOnce();
    await transport.close();
    await vi.advanceTimersByTimeAsync(0);
    expect(result()).toEqual(new Error('TCP/FIPS transport is closed'));
    release(connection);
    await vi.advanceTimersByTimeAsync(6_000);
    expect(abort).toHaveBeenCalledExactlyOnceWith(connection);
    expect(connect).toHaveBeenCalledOnce();
    expect(write).not.toHaveBeenCalled();
  });
});
