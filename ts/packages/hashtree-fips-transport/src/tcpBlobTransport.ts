import { FipsTcpEndpoint, State, type ConnectionId, type FipsDatagramEndpoint } from '@fips/tcp';
import { sha256, type Hash, type Store } from '@hashtree/core';

export const TCP_BLOB_SERVICE_PORT = 39_018;
export const TCP_BLOB_MAGIC = 0x48;
export const TCP_BLOB_VERSION = 1;
const GET = 1;
const MISSING = 0;
const FOUND = 1;
const HEADER_BYTES = 7;
const REQUEST_BYTES = 35;
const IO_CHUNK_BYTES = 64 * 1024;
const POLL_INTERVAL_MS = 10;
export const TCP_BLOB_MAX_BYTES = 16 * 1024 * 1024;

export interface TcpBlobTransportOptions {
  endpoint: FipsDatagramEndpoint;
  localStore: Store;
  timeoutMs?: number;
}

/** Hash-verified Hashtree blobs carried by one reliable TCP/FIPS stream per request. */
export class TcpBlobTransport {
  private readonly tcp: FipsTcpEndpoint;
  private readonly timeoutMs: number;
  private readonly timer: ReturnType<typeof setInterval>;
  private pumping = false;
  private closed = false;

  constructor(private readonly options: TcpBlobTransportOptions) {
    this.timeoutMs = options.timeoutMs ?? 5_500;
    this.tcp = new FipsTcpEndpoint(options.endpoint, TCP_BLOB_SERVICE_PORT, {
      sendBuffer: 1024 * 1024,
      receiveBuffer: 0xffff,
    });
    this.timer = setInterval(() => void this.pump(), POLL_INTERVAL_MS);
  }

  async get(hash: Hash, peerIds: readonly string[]): Promise<Uint8Array | null> {
    const local = await this.verifiedGet(hash);
    if (local) return local;
    const peers = [...new Set(peerIds.map((peer) => peer.trim()).filter(Boolean))];
    if (peers.length === 0) throw new Error('no TCP/FIPS blob providers are available');
    let failures: unknown[] = [];
    for (let sessionAttempt = 0; sessionAttempt < 2; sessionAttempt += 1) {
      const results = await Promise.all(
        peers.map(async (peer) => {
          try {
            const data = await this.fetchFromPeer(
              peer,
              hash,
              Math.max(250, Math.floor(this.timeoutMs / 2)),
            );
            return data === null
              ? { kind: 'missing' as const }
              : { kind: 'found' as const, data };
          } catch (error) {
            return { kind: 'failed' as const, error };
          }
        }),
      );
      const found = results.find((result) => result.kind === 'found');
      if (found?.kind === 'found') return found.data;
      failures = results.flatMap((result) => result.kind === 'failed' ? [result.error] : []);
      if (failures.length === 0) return null;
    }
    throw new AggregateError(failures, 'TCP/FIPS blob availability is uncertain');
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    clearInterval(this.timer);
    await this.tcp.dispose();
  }

  private async fetchFromPeer(
    peer: string,
    hash: Hash,
    attemptTimeoutMs: number,
  ): Promise<Uint8Array | null> {
    const deadline = Date.now() + attemptTimeoutMs;
    const connection = await this.tcp.connect(peer);
    try {
      await this.waitEstablished(connection, deadline);
      const request = encodeTcpBlobRequest(hash);
      await this.writeAll(connection, request, deadline);
      const header = await this.readExact(connection, HEADER_BYTES, deadline);
      const response = decodeTcpBlobResponseHeader(header);
      if (!response.found) return null;
      const data = await this.readExact(connection, response.size, deadline);
      if (!bytesEqual(await sha256(data), hash)) throw new Error('TCP/FIPS blob hash mismatch');
      await this.options.localStore.put(hash, data.slice()).catch(() => false);
      return data;
    } finally {
      await this.tcp.close(connection).catch(() => undefined);
    }
  }

  private async pump(): Promise<void> {
    if (this.closed || this.pumping) return;
    this.pumping = true;
    try {
      await this.tcp.poll();
      for (;;) {
        const connection = await this.tcp.accept();
        if (connection === undefined) break;
        void this.serve(connection).catch(() => this.tcp.close(connection).catch(() => undefined));
      }
    } finally {
      this.pumping = false;
    }
  }

  private async serve(connection: ConnectionId): Promise<void> {
    const deadline = Date.now() + this.timeoutMs;
    const request = await this.readExact(connection, REQUEST_BYTES, deadline);
    if (request[0] !== TCP_BLOB_MAGIC || request[1] !== TCP_BLOB_VERSION || request[2] !== GET) {
      throw new Error('invalid TCP/FIPS blob request');
    }
    const hash = request.slice(3) as Hash;
    const data = await this.verifiedGet(hash);
    const header = encodeTcpBlobResponseHeader(Boolean(data), data?.byteLength ?? 0);
    await this.writeAll(connection, header, deadline);
    if (data) await this.writeAll(connection, data, deadline);
    await this.tcp.close(connection).catch(() => undefined);
  }

  private async verifiedGet(hash: Hash): Promise<Uint8Array | null> {
    const data = await this.options.localStore.get(hash);
    if (!data || !bytesEqual(await sha256(data), hash)) return null;
    return data;
  }

  private async waitEstablished(connection: ConnectionId, deadline: number): Promise<void> {
    while (Date.now() < deadline) {
      if (await this.tcp.state(connection) === State.Established) return;
      await sleep(POLL_INTERVAL_MS);
    }
    throw new Error('TCP/FIPS connect timed out');
  }

  private async writeAll(connection: ConnectionId, data: Uint8Array, deadline: number): Promise<void> {
    let offset = 0;
    while (offset < data.byteLength && Date.now() < deadline) {
      const end = Math.min(offset + IO_CHUNK_BYTES, data.byteLength);
      const accepted = await this.tcp.write(connection, data.subarray(offset, end));
      offset += accepted;
      if (accepted === 0) await sleep(POLL_INTERVAL_MS);
    }
    if (offset !== data.byteLength) throw new Error('TCP/FIPS write timed out');
  }

  private async readExact(connection: ConnectionId, size: number, deadline: number): Promise<Uint8Array> {
    const chunks: Uint8Array[] = [];
    let received = 0;
    while (received < size && Date.now() < deadline) {
      const chunk = await this.tcp.read(connection, Math.min(IO_CHUNK_BYTES, size - received));
      if (chunk.byteLength > 0) {
        chunks.push(chunk);
        received += chunk.byteLength;
      } else {
        if (await this.tcp.isReadClosed(connection)) break;
        await sleep(POLL_INTERVAL_MS);
      }
    }
    if (received !== size) throw new Error('TCP/FIPS read timed out');
    const out = new Uint8Array(size);
    let offset = 0;
    for (const chunk of chunks) {
      out.set(chunk, offset);
      offset += chunk.byteLength;
    }
    return out;
  }
}

export function encodeTcpBlobRequest(hash: Uint8Array): Uint8Array {
  if (hash.byteLength !== 32) throw new Error('TCP/FIPS blob hash must be 32 bytes');
  const request = new Uint8Array(REQUEST_BYTES);
  request.set([TCP_BLOB_MAGIC, TCP_BLOB_VERSION, GET]);
  request.set(hash, 3);
  return request;
}

export function encodeTcpBlobResponseHeader(found: boolean, size: number): Uint8Array {
  if (!Number.isInteger(size) || size < 0 || size > TCP_BLOB_MAX_BYTES) {
    throw new Error('TCP/FIPS blob response size is invalid');
  }
  if (!found && size !== 0) throw new Error('missing TCP/FIPS blob response has non-zero size');
  const header = new Uint8Array(HEADER_BYTES);
  header.set([TCP_BLOB_MAGIC, TCP_BLOB_VERSION, found ? FOUND : MISSING]);
  new DataView(header.buffer).setUint32(3, size);
  return header;
}

export interface TcpBlobResponseHeader {
  found: boolean;
  size: number;
}

export function decodeTcpBlobResponseHeader(header: Uint8Array): TcpBlobResponseHeader {
  if (
    header.byteLength !== HEADER_BYTES
    || header[0] !== TCP_BLOB_MAGIC
    || header[1] !== TCP_BLOB_VERSION
  ) {
    throw new Error('invalid TCP/FIPS blob response');
  }
  const status = header[2];
  const size = new DataView(header.buffer, header.byteOffset + 3, 4).getUint32(0);
  if (status === MISSING) {
    if (size !== 0) throw new Error('missing TCP/FIPS blob response has non-zero size');
    return { found: false, size };
  }
  if (status !== FOUND) throw new Error('unsupported TCP/FIPS blob response status');
  if (size > TCP_BLOB_MAX_BYTES) throw new Error('TCP/FIPS blob exceeds size limit');
  return { found: true, size };
}

function bytesEqual(left: Uint8Array, right: Uint8Array): boolean {
  if (left.byteLength !== right.byteLength) return false;
  return left.every((byte, index) => byte === right[index]);
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
