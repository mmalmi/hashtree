import { afterEach, describe, expect, it, vi } from 'vitest';
import { HashtreeWorkerClient } from '../src/client.js';
import type { WorkerRequest, WorkerResponse } from '../src/protocol.js';

class FakeWorker {
  onmessage: ((event: MessageEvent<WorkerResponse>) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;
  postedMessages: Array<{ message: WorkerRequest; transfer?: Transferable[] }> = [];
  private streamCounter = 0;

  postMessage(message: WorkerRequest, transfer?: Transferable[]): void {
    this.postedMessages.push({ message, transfer });
    if (message.type === 'init') {
      this.emitMessage({ type: 'ready', id: message.id });
      return;
    }

    if (message.type === 'beginPutBlobStream') {
      this.streamCounter += 1;
      this.emitMessage({ type: 'blobStreamStarted', id: message.id, streamId: `stream-${this.streamCounter}` });
      return;
    }

    if (message.type === 'putBlock') {
      this.emitMessage({
        type: 'blockStored',
        id: message.id,
        block: { hashHex: message.hashHex ?? 'abc', nhash: 'nhash1abc' },
      });
      return;
    }

    if (message.type === 'putBlocks') {
      this.emitMessage({
        type: 'blocksStored',
        id: message.id,
        blocks: message.blocks.map((block, index) => ({
          hashHex: block.hashHex ?? `hash-${index}`,
          nhash: `nhash1${index}`,
        })),
      });
      return;
    }

    if (message.type === 'appendPutBlobStream' || message.type === 'cancelPutBlobStream') {
      this.emitMessage({ type: 'void', id: message.id });
      return;
    }

    if (message.type === 'finishPutBlobStream') {
      this.emitMessage({ type: 'blobStored', id: message.id, hashHex: 'abc', nhash: 'nhash1abc' });
      return;
    }

    if (message.type === 'close') {
      this.emitMessage({ type: 'void', id: message.id });
    }
  }

  terminate(): void {
    // no-op for tests
  }

  emitMessage(message: WorkerResponse): void {
    this.onmessage?.({ data: message } as MessageEvent<WorkerResponse>);
  }
}

class GetBlobWorker extends FakeWorker {
  static instance: GetBlobWorker | null = null;

  constructor() {
    super();
    GetBlobWorker.instance = this;
  }

  override postMessage(message: WorkerRequest, transfer?: Transferable[]): void {
    super.postMessage(message, transfer);
    if (message.type === 'getBlob') {
      this.emitMessage({
        type: 'blob',
        id: message.id,
        data: new Uint8Array([1]),
        source: 'blossom',
      });
    }
    if (message.type === 'hasBlob') {
      this.emitMessage({
        type: 'availability',
        id: message.id,
        available: true,
        size: 42,
        source: 'blossom',
      });
    }
  }
}

describe('HashtreeWorkerClient timeouts', () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it('does not timeout putBlob at the default request timeout window', async () => {
    vi.useFakeTimers();
    const client = new HashtreeWorkerClient(FakeWorker as unknown as new () => Worker);

    let putError: Error | undefined;
    void client.putBlob(new Uint8Array([1, 2, 3]), 'application/octet-stream', false).catch((err) => {
      putError = err as Error;
    });

    await vi.advanceTimersByTimeAsync(30_001);
    expect(putError).toBeUndefined();

    await vi.runOnlyPendingTimersAsync();
    expect(putError?.message).toContain('Worker request timed out: putBlob');

    await client.close();
  });

  it('keeps default timeout for other requests like getBlob', async () => {
    vi.useFakeTimers();
    const client = new HashtreeWorkerClient(FakeWorker as unknown as new () => Worker);

    let getError: Error | undefined;
    void client.getBlob('deadbeef').catch((err) => {
      getError = err as Error;
    });

    await vi.advanceTimersByTimeAsync(30_001);
    expect(getError?.message).toContain('Worker request timed out: getBlob');

    await client.close();
  });

  it('passes scoped read sources to getBlob', async () => {
    const client = new HashtreeWorkerClient(GetBlobWorker as unknown as new () => Worker);

    await client.getBlob('deadbeef', { sourceIds: ['blossom'] });

    expect(GetBlobWorker.instance?.postedMessages.at(-1)?.message).toMatchObject({
      type: 'getBlob',
      hashHex: 'deadbeef',
      sourceIds: ['blossom'],
    });
    await client.close();
  });

  it('checks scoped blob availability without downloading it', async () => {
    const client = new HashtreeWorkerClient(GetBlobWorker as unknown as new () => Worker);

    await expect(client.hasBlob('cafebabe', { sourceIds: ['blossom'] })).resolves.toEqual({
      available: true,
      size: 42,
      source: 'blossom',
    });

    expect(GetBlobWorker.instance?.postedMessages.at(-1)?.message).toMatchObject({
      type: 'hasBlob',
      hashHex: 'cafebabe',
      sourceIds: ['blossom'],
    });
    await client.close();
  });

  it('supports streamed putBlob lifecycle', async () => {
    const client = new HashtreeWorkerClient(FakeWorker as unknown as new () => Worker);
    const streamId = await client.beginPutBlobStream('application/octet-stream');
    expect(streamId).toBe('stream-1');

    await client.appendPutBlobStream(streamId, new Uint8Array([1, 2, 3]));
    const stored = await client.finishPutBlobStream(streamId);
    expect(stored).toEqual({ hashHex: 'abc', nhash: 'nhash1abc' });

    await client.cancelPutBlobStream(streamId);
    await client.close();
  });

  it('stores raw blocks through the worker protocol', async () => {
    const client = new HashtreeWorkerClient(FakeWorker as unknown as new () => Worker);
    const single = await client.putBlock(new Uint8Array([1, 2, 3]), {
      hashHex: 'a'.repeat(64),
      upload: true,
    });
    expect(single).toEqual({ hashHex: 'a'.repeat(64), nhash: 'nhash1abc' });

    const many = await client.putBlocks([
      { data: new Uint8Array([4]), hashHex: 'b'.repeat(64) },
      { data: new Uint8Array([5]), hashHex: 'c'.repeat(64) },
    ], { upload: true });
    expect(many).toEqual([
      { hashHex: 'b'.repeat(64), nhash: 'nhash10' },
      { hashHex: 'c'.repeat(64), nhash: 'nhash11' },
    ]);

    await client.close();
  });

  it('clones p2p fetch handler bytes before transferring them to the worker', async () => {
    const worker = new FakeWorker();
    const WorkerFactory = class {
      constructor() {
        return worker;
      }
    } as unknown as new () => Worker;
    const client = new HashtreeWorkerClient(WorkerFactory);
    const sourceData = new Uint8Array([1, 2, 3]);

    client.setP2PFetchHandler(async () => sourceData);
    await client.init();

    worker.emitMessage({
      type: 'p2pFetch',
      requestId: 'req-1',
      hashHex: 'deadbeef',
    });
    await Promise.resolve();
    await Promise.resolve();

    const posted = worker.postedMessages.find((entry) => entry.message.type === 'p2pFetchResult');
    expect(posted?.message.type).toBe('p2pFetchResult');
    expect(sourceData.buffer.byteLength).toBe(3);
    if (posted?.message.type === 'p2pFetchResult' && posted.message.data) {
      expect(posted.message.data).toEqual(new Uint8Array([1, 2, 3]));
      expect(posted.message.data).not.toBe(sourceData);
    }

    await client.close();
  });

  it('returns the current p2p peer list to the worker when requested', async () => {
    const worker = new FakeWorker();
    const WorkerFactory = class {
      constructor() {
        return worker;
      }
    } as unknown as new () => Worker;
    const client = new HashtreeWorkerClient(WorkerFactory);

    client.setP2PPeerListHandler(() => ['peer-a', 'peer-b']);
    await client.init();

    worker.emitMessage({
      type: 'p2pPeerList',
      requestId: 'peers-1',
    });
    await Promise.resolve();
    await Promise.resolve();

    expect(worker.postedMessages).toContainEqual({
      message: {
        type: 'p2pPeerListResult',
        id: expect.any(String),
        requestId: 'peers-1',
        peerIds: ['peer-a', 'peer-b'],
      },
      transfer: undefined,
    });

    await client.close();
  });

  it('preserves FIPS provider method receivers', async () => {
    const worker = new FakeWorker();
    const WorkerFactory = class {
      constructor() {
        return worker;
      }
    } as unknown as new () => Worker;
    const client = new HashtreeWorkerClient(WorkerFactory);
    class Provider {
      readonly peerIds = ['fips-peer'];

      async fetch(): Promise<Uint8Array> {
        return new Uint8Array([this.peerIds.length]);
      }

      listPeerIds(): string[] {
        return this.peerIds;
      }
    }
    client.setP2PProvider(new Provider());
    await client.init();

    worker.emitMessage({ type: 'p2pFetch', requestId: 'bound-fetch', hashHex: 'ab'.repeat(32) });
    worker.emitMessage({ type: 'p2pPeerList', requestId: 'bound-peers' });
    await vi.waitFor(() => {
      expect(worker.postedMessages.some(({ message }) => (
        message.type === 'p2pFetchResult' && message.data?.[0] === 1
      ))).toBe(true);
      expect(worker.postedMessages.some(({ message }) => (
        message.type === 'p2pPeerListResult' && message.peerIds?.[0] === 'fips-peer'
      ))).toBe(true);
    });

    await client.close();
  });
});
