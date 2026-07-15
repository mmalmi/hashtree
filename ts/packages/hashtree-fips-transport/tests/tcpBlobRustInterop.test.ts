import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { createSocket, type Socket as DatagramSocket } from 'node:dgram';
import { once } from 'node:events';
import { createInterface } from 'node:readline';
import path from 'node:path';
import {
  encodeNpub,
  FipsNode,
  identityFromSecretKey,
  type Transport,
  type TransportAddress,
  type TransportContext,
} from '@fips/core';
import { fromHex, MemoryStore, sha256, toHex, type Hash } from '@hashtree/core';
import { describe, expect, it } from 'vitest';
import { TcpBlobTransport } from '../src/tcpBlobTransport.js';

interface ReadyMessage {
  type: 'ready';
  peerId: string;
  hash: string;
  data: string;
  largeHash: string;
  largeData: string;
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

type FixtureMessage = ReadyMessage | FetchResultMessage | ErrorMessage;

/** A loopback-only UDP underlay; FIPS still owns authentication and framing. */
class LoopbackUdpTransport implements Transport {
  readonly type = 'udp';
  readonly mtu = 1_200;
  private readonly socket: DatagramSocket = createSocket('udp4');
  private context?: TransportContext;
  private bound = false;

  async start(context: TransportContext): Promise<void> {
    this.context = context;
    this.socket.on('message', (packet, remote) => {
      context.onPacket({
        transportType: this.type,
        remoteAddr: {
          transport: this.type,
          addr: `${remote.address}:${remote.port}`,
        },
        data: new Uint8Array(packet),
        receivedAtMs: Date.now(),
      });
    });
    await new Promise<void>((resolve, reject) => {
      const onError = (error: Error): void => reject(error);
      this.socket.once('error', onError);
      this.socket.bind(0, '127.0.0.1', () => {
        this.socket.off('error', onError);
        this.bound = true;
        resolve();
      });
    });
  }

  async stop(): Promise<void> {
    this.context = undefined;
    if (!this.bound) return;
    this.bound = false;
    await new Promise<void>((resolve) => this.socket.close(() => resolve()));
  }

  async connect(remote: TransportAddress): Promise<void> {
    parseAddress(remote.addr);
    this.context?.onConnectionState?.({ remoteAddr: remote, state: 'connected' });
  }

  async send(remote: TransportAddress, packet: Uint8Array): Promise<void> {
    const { host, port } = parseAddress(remote.addr);
    await new Promise<void>((resolve, reject) => {
      this.socket.send(packet, port, host, (error) => error ? reject(error) : resolve());
    });
  }

  async close(remote: TransportAddress): Promise<void> {
    this.context?.onConnectionState?.({ remoteAddr: remote, state: 'disconnected' });
  }

  localAddress(): string {
    const address = this.socket.address();
    if (typeof address === 'string') throw new Error('expected an IP UDP socket');
    return `${address.address}:${address.port}`;
  }
}

class RustInteropFixture {
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
  ) {}

  static async start(peerNpub: string, peerAddress: string): Promise<RustInteropFixture> {
    const proc = spawn(
      'cargo',
      [
        ...candidateCargoPatchArgs(),
        'run',
        '--locked',
        '-q',
        '-p',
        'hashtree-fips-transport',
        '--no-default-features',
        '--features',
        'interop-fixture',
        '--bin',
        'hashtree-fips-stdio-fixture',
        '--',
        peerNpub,
        peerAddress,
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
    let ready: ReadyMessage;
    try {
      ready = await new Promise<ReadyMessage>((resolve, reject) => {
        const timer = setTimeout(() => {
          reject(new Error(`Rust TCP blob fixture did not become ready\n${stderr}`));
        }, 120_000);
        proc.once('exit', (code, signal) => {
          clearTimeout(timer);
          reject(new Error(`Rust TCP blob fixture exited code=${code} signal=${signal}\n${stderr}`));
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
    } catch (error) {
      await stopProcess(proc);
      throw error;
    }

    const fixture = new RustInteropFixture(proc, ready);
    fixture.stderr = stderr;
    proc.stderr.on('data', (chunk) => {
      fixture.stderr = trimLog(fixture.stderr + chunk.toString('utf8'));
    });
    lines.on('line', (line) => fixture.handleLine(line));
    return fixture;
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
    this.process.stdin.write(`${JSON.stringify({ type: 'fetch', id, hash: toHex(hash) })}\n`);
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

describe('Rust/TypeScript TCP blob v1 interop', () => {
  it('exchanges hits and explicit misses through real TCP/FIPS transports', async () => {
    const identity = await identityFromSecretKey(secret(7));
    const udp = new LoopbackUdpTransport();
    const node = new FipsNode({
      identity,
      transports: [udp],
      routingMode: 'reply_learned',
    });
    const tsStore = new MemoryStore();
    const tsData = new TextEncoder().encode('typescript TCP blob v1 fixture blob');
    const tsHash = await sha256(tsData);
    const tsLargeData = largeBlob(180_013);
    const tsLargeHash = await sha256(tsLargeData);
    await tsStore.put(tsHash, tsData);
    await tsStore.put(tsLargeHash, tsLargeData);
    await node.start();
    const transport = new TcpBlobTransport({ endpoint: node, localStore: tsStore, timeoutMs: 10_000 });
    let fixture: RustInteropFixture | undefined;
    const missing = new Uint8Array(32).fill(0x5a);

    try {
      fixture = await RustInteropFixture.start(
        encodeNpub(identity.xOnlyPubkey),
        udp.localAddress(),
      );
      await expect(
        transport.get(fromHex(fixture.ready.hash), [fixture.ready.peerId]),
      ).resolves.toEqual(fromHex(fixture.ready.data));
      await expect(
        transport.get(fromHex(fixture.ready.largeHash), [fixture.ready.peerId]),
      ).resolves.toEqual(fromHex(fixture.ready.largeData));
      await expect(fixture.fetch(tsHash)).resolves.toBe(toHex(tsData));
      await expect(fixture.fetch(tsLargeHash)).resolves.toBe(toHex(tsLargeData));
      await expect(transport.get(missing, [fixture.ready.peerId])).resolves.toBeNull();
      await expect(fixture.fetch(missing)).resolves.toBeNull();
    } finally {
      await transport.close();
      if (fixture) await fixture.close();
      await node.stop();
    }
  }, 180_000);
});

function repoRoot(): string {
  return path.resolve(__dirname, '../../../..');
}

function cargoRoot(): string {
  return path.join(repoRoot(), 'rust');
}

function candidateCargoPatchArgs(): string[] {
  const fipsDir = process.env.FIPS_DIR?.trim();
  const fipsTcpDir = process.env.FIPS_TCP_DIR?.trim();
  if (!fipsDir && !fipsTcpDir) return [];
  if (!fipsDir || !fipsTcpDir) {
    throw new Error('FIPS_DIR and FIPS_TCP_DIR must be set together');
  }
  const patch = (crateName: string, cratePath: string): string[] => [
    '--config',
    `patch.crates-io.${crateName}.path=${JSON.stringify(cratePath)}`,
  ];
  return [
    ...patch('fips-core', path.join(fipsDir, 'crates/fips-core')),
    ...patch('fips-tcp', path.join(fipsTcpDir, 'rust/fips-tcp')),
    ...patch('fips-tcp-endpoint', path.join(fipsTcpDir, 'rust/fips-tcp-endpoint')),
  ];
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

function largeBlob(length: number): Uint8Array {
  return Uint8Array.from({ length }, (_, index) => index % 251);
}

function secret(value: number): Uint8Array {
  const key = new Uint8Array(32);
  key[31] = value;
  return key;
}

function parseAddress(value: string): { host: string; port: number } {
  const separator = value.lastIndexOf(':');
  const host = value.slice(0, separator);
  const port = Number(value.slice(separator + 1));
  if (separator <= 0 || host !== '127.0.0.1' || !Number.isInteger(port) || port <= 0) {
    throw new Error(`invalid loopback UDP address ${value}`);
  }
  return { host, port };
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
