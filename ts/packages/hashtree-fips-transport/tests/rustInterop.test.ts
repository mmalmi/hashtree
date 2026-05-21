import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { once } from 'node:events';
import { createInterface } from 'node:readline';
import path from 'node:path';
import { describe, expect, it } from 'vitest';
import { fromHex, MemoryStore, sha256, toHex, type Hash } from '@hashtree/core';
import {
  HashtreeFipsTransport,
  type FipsEndpoint,
  type FipsEndpointMessage,
} from '../src/index.js';

interface ReadyMessage {
  type: 'ready';
  peerId: string;
  hash: string;
  data: string;
}

interface FrameMessage {
  type: 'frame';
  peerId: string;
  data: string;
}

interface FetchResultMessage {
  type: 'fetchResult';
  id: string;
  data: string | null;
}

interface ErrorMessage {
  type: 'error';
  message: string;
}

type FixtureMessage = ReadyMessage | FrameMessage | FetchResultMessage | ErrorMessage;

class RustFixtureEndpoint implements FipsEndpoint {
  private readonly handlers = new Set<(message: FipsEndpointMessage) => void | Promise<void>>();

  constructor(
    private readonly fixture: RustInteropFixture,
    private readonly peerId: string,
  ) {}

  localPeerId(): string {
    return 'ts';
  }

  listPeerIds(): string[] {
    return [this.peerId];
  }

  async send(peerId: string, data: Uint8Array): Promise<void> {
    if (peerId !== this.peerId) {
      throw new Error(`unknown rust fixture peer ${peerId}`);
    }
    this.fixture.write({
      type: 'frame',
      data: toHex(data as Hash),
    });
  }

  onMessage(handler: (message: FipsEndpointMessage) => void | Promise<void>): () => void {
    this.handlers.add(handler);
    return () => {
      this.handlers.delete(handler);
    };
  }

  deliver(message: FrameMessage): void {
    const data = fromHex(message.data as never);
    for (const handler of this.handlers) {
      void Promise.resolve(handler({
        peerId: message.peerId,
        data,
      }));
    }
  }
}

class RustInteropFixture {
  readonly endpoint: RustFixtureEndpoint;
  private stderr = '';
  private readonly fetchResolvers = new Map<string, {
    resolve: (data: string | null) => void;
    reject: (error: Error) => void;
    timer: ReturnType<typeof setTimeout>;
  }>();
  private fetchSeq = 0;

  private constructor(
    readonly process: ChildProcessWithoutNullStreams,
    readonly ready: ReadyMessage,
  ) {
    this.endpoint = new RustFixtureEndpoint(this, ready.peerId);
  }

  static async start(): Promise<RustInteropFixture> {
    const proc = spawn(
      'cargo',
      [
        'run',
        '-q',
        '-p',
        'hashtree-fips-transport',
        '--bin',
        'hashtree-fips-stdio-fixture',
      ],
      {
        cwd: cargoRoot(),
        detached: process.platform !== 'win32',
      },
    );

    let stderr = '';
    proc.stderr.on('data', (chunk) => {
      stderr = trimLog(stderr + chunk.toString('utf8'));
    });

    const lines = createInterface({ input: proc.stdout });
    const ready = await new Promise<ReadyMessage>((resolve, reject) => {
      const timer = setTimeout(() => {
        reject(new Error(`Rust hashtree FIPS fixture did not become ready\n${stderr}`));
      }, 120_000);
      proc.once('exit', (code, signal) => {
        clearTimeout(timer);
        reject(new Error(`Rust hashtree FIPS fixture exited code=${code} signal=${signal}\n${stderr}`));
      });
      lines.on('line', (line) => {
        const message = parseFixtureMessage(line);
        if (!message) return;
        if (message.type === 'ready') {
          clearTimeout(timer);
          resolve(message);
        } else if (message.type === 'error') {
          clearTimeout(timer);
          reject(new Error(message.message));
        }
      });
    });

    const fixture = new RustInteropFixture(proc, ready);
    fixture.stderr = stderr;
    proc.stderr.on('data', (chunk) => {
      fixture.stderr = trimLog(fixture.stderr + chunk.toString('utf8'));
    });
    lines.on('line', (line) => fixture.handleLine(line));
    return fixture;
  }

  write(message: Record<string, unknown>): void {
    this.process.stdin.write(`${JSON.stringify(message)}\n`);
  }

  fetch(hash: Hash): Promise<string | null> {
    const id = `fetch-${++this.fetchSeq}`;
    const pending = new Promise<string | null>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.fetchResolvers.delete(id);
        reject(new Error(`Rust fixture fetch timed out for ${toHex(hash)}\n${this.stderr}`));
      }, 10_000);
      this.fetchResolvers.set(id, { resolve, reject, timer });
    });
    this.write({ type: 'fetch', id, hash: toHex(hash) });
    return pending;
  }

  async close(): Promise<void> {
    for (const pending of this.fetchResolvers.values()) {
      clearTimeout(pending.timer);
      pending.reject(new Error('fixture closed'));
    }
    this.fetchResolvers.clear();
    await stopProcess(this.process);
  }

  private handleLine(line: string): void {
    const message = parseFixtureMessage(line);
    if (!message) return;
    if (message.type === 'frame') {
      this.endpoint.deliver(message);
      return;
    }
    if (message.type === 'fetchResult') {
      const pending = this.fetchResolvers.get(message.id);
      if (!pending) return;
      this.fetchResolvers.delete(message.id);
      clearTimeout(pending.timer);
      pending.resolve(message.data);
      return;
    }
    if (message.type === 'error') {
      for (const pending of this.fetchResolvers.values()) {
        clearTimeout(pending.timer);
        pending.reject(new Error(message.message));
      }
      this.fetchResolvers.clear();
    }
  }
}

describe('Rust hashtree FIPS transport interop', () => {
  it('exchanges hash-verified blobs between TypeScript and Rust transports', async () => {
    const fixture = await RustInteropFixture.start();
    const tsStore = new MemoryStore();
    const tsData = new TextEncoder().encode('typescript hashtree fips transport fixture blob');
    const tsHash = await sha256(tsData);
    await tsStore.put(tsHash, tsData);

    const transport = new HashtreeFipsTransport({
      endpoint: fixture.endpoint,
      localStore: tsStore,
      peers: [fixture.ready.peerId],
      requestTimeoutMs: 10_000,
    });

    try {
      await expect(transport.get(fromHex(fixture.ready.hash as never))).resolves.toEqual(
        fromHex(fixture.ready.data as never),
      );
      await expect(fixture.fetch(tsHash)).resolves.toBe(toHex(tsData as Hash));
    } finally {
      transport.close();
      await fixture.close();
    }
  }, 180_000);
});

function repoRoot(): string {
  return path.resolve(__dirname, '../../../..');
}

function cargoRoot(): string {
  return path.join(repoRoot(), 'rust');
}

function parseFixtureMessage(line: string): FixtureMessage | null {
  try {
    const message = JSON.parse(line) as FixtureMessage;
    if (message && typeof message === 'object' && typeof message.type === 'string') {
      return message;
    }
  } catch {
    return null;
  }
  return null;
}

async function stopProcess(proc: ChildProcessWithoutNullStreams): Promise<void> {
  if (proc.exitCode !== null || proc.signalCode !== null) return;
  proc.stdin.end();
  if (process.platform !== 'win32' && proc.pid) {
    try {
      process.kill(-proc.pid, 'SIGTERM');
    } catch {
      proc.kill('SIGTERM');
    }
  } else {
    proc.kill('SIGTERM');
  }
  const exited = once(proc, 'exit').then(() => undefined);
  const timeout = new Promise<void>((resolve) => setTimeout(resolve, 5_000));
  await Promise.race([exited, timeout]);
  if (proc.exitCode === null && proc.signalCode === null) {
    proc.kill('SIGKILL');
    await exited.catch(() => undefined);
  }
}

function trimLog(log: string): string {
  const max = 24_000;
  return log.length > max ? log.slice(log.length - max) : log;
}
