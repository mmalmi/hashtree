import { FipsTcpEndpoint, State } from '@fips/tcp';
import { sha256 } from '@hashtree/core';
export const TCP_BLOB_SERVICE_PORT = 39_018;
export const TCP_BLOB_MAGIC = 0x48;
export const TCP_BLOB_VERSION = 1;
const GET = 1;
const FOUND = 1;
const HEADER_BYTES = 7;
const REQUEST_BYTES = 35;
const IO_CHUNK_BYTES = 64 * 1024;
const POLL_INTERVAL_MS = 10;
export const TCP_BLOB_MAX_BYTES = 16 * 1024 * 1024;
/** Hash-verified Hashtree blobs carried by one reliable TCP/FIPS stream per request. */
export class TcpBlobTransport {
    options;
    tcp;
    timeoutMs;
    timer;
    pumping = false;
    closed = false;
    constructor(options) {
        this.options = options;
        this.timeoutMs = options.timeoutMs ?? 5_500;
        this.tcp = new FipsTcpEndpoint(options.endpoint, TCP_BLOB_SERVICE_PORT, {
            sendBuffer: 1024 * 1024,
            receiveBuffer: 0xffff,
        });
        this.timer = setInterval(() => void this.pump(), POLL_INTERVAL_MS);
    }
    async get(hash, peerIds) {
        const local = await this.verifiedGet(hash);
        if (local)
            return local;
        const peers = [...new Set(peerIds.map((peer) => peer.trim()).filter(Boolean))];
        if (peers.length === 0)
            return null;
        for (let sessionAttempt = 0; sessionAttempt < 2; sessionAttempt += 1) {
            const results = await Promise.all(peers.map((peer) => this.fetchFromPeer(peer, hash, Math.max(250, Math.floor(this.timeoutMs / 2))).catch(() => undefined)));
            const data = results.find((result) => result instanceof Uint8Array);
            if (data)
                return data;
            if (results.every((result) => result === null))
                return null;
        }
        return null;
    }
    async close() {
        if (this.closed)
            return;
        this.closed = true;
        clearInterval(this.timer);
        await this.tcp.dispose();
    }
    async fetchFromPeer(peer, hash, attemptTimeoutMs) {
        const deadline = Date.now() + attemptTimeoutMs;
        const connection = await this.tcp.connect(peer);
        try {
            await this.waitEstablished(connection, deadline);
            const request = encodeTcpBlobRequest(hash);
            await this.writeAll(connection, request, deadline);
            const header = await this.readExact(connection, HEADER_BYTES, deadline);
            if (header[0] !== TCP_BLOB_MAGIC || header[1] !== TCP_BLOB_VERSION)
                throw new Error('invalid TCP/FIPS blob response');
            if (header[2] !== FOUND)
                return null;
            const size = new DataView(header.buffer, header.byteOffset + 3, 4).getUint32(0);
            if (size > TCP_BLOB_MAX_BYTES)
                throw new Error('TCP/FIPS blob exceeds size limit');
            const data = await this.readExact(connection, size, deadline);
            if (!bytesEqual(await sha256(data), hash))
                throw new Error('TCP/FIPS blob hash mismatch');
            await this.options.localStore.put(hash, data.slice()).catch(() => false);
            return data;
        }
        finally {
            await this.tcp.close(connection).catch(() => undefined);
        }
    }
    async pump() {
        if (this.closed || this.pumping)
            return;
        this.pumping = true;
        try {
            await this.tcp.poll();
            for (;;) {
                const connection = await this.tcp.accept();
                if (connection === undefined)
                    break;
                void this.serve(connection).catch(() => this.tcp.close(connection).catch(() => undefined));
            }
        }
        finally {
            this.pumping = false;
        }
    }
    async serve(connection) {
        const deadline = Date.now() + this.timeoutMs;
        const request = await this.readExact(connection, REQUEST_BYTES, deadline);
        if (request[0] !== TCP_BLOB_MAGIC || request[1] !== TCP_BLOB_VERSION || request[2] !== GET) {
            throw new Error('invalid TCP/FIPS blob request');
        }
        const hash = request.slice(3);
        const data = await this.verifiedGet(hash);
        const header = encodeTcpBlobResponseHeader(Boolean(data), data?.byteLength ?? 0);
        await this.writeAll(connection, header, deadline);
        if (data)
            await this.writeAll(connection, data, deadline);
        await this.tcp.close(connection).catch(() => undefined);
    }
    async verifiedGet(hash) {
        const data = await this.options.localStore.get(hash);
        if (!data || !bytesEqual(await sha256(data), hash))
            return null;
        return data;
    }
    async waitEstablished(connection, deadline) {
        while (Date.now() < deadline) {
            if (await this.tcp.state(connection) === State.Established)
                return;
            await sleep(POLL_INTERVAL_MS);
        }
        throw new Error('TCP/FIPS connect timed out');
    }
    async writeAll(connection, data, deadline) {
        let offset = 0;
        while (offset < data.byteLength && Date.now() < deadline) {
            const end = Math.min(offset + IO_CHUNK_BYTES, data.byteLength);
            const accepted = await this.tcp.write(connection, data.subarray(offset, end));
            offset += accepted;
            if (accepted === 0)
                await sleep(POLL_INTERVAL_MS);
        }
        if (offset !== data.byteLength)
            throw new Error('TCP/FIPS write timed out');
    }
    async readExact(connection, size, deadline) {
        const chunks = [];
        let received = 0;
        while (received < size && Date.now() < deadline) {
            const chunk = await this.tcp.read(connection, Math.min(IO_CHUNK_BYTES, size - received));
            if (chunk.byteLength > 0) {
                chunks.push(chunk);
                received += chunk.byteLength;
            }
            else {
                if (await this.tcp.isReadClosed(connection))
                    break;
                await sleep(POLL_INTERVAL_MS);
            }
        }
        if (received !== size)
            throw new Error('TCP/FIPS read timed out');
        const out = new Uint8Array(size);
        let offset = 0;
        for (const chunk of chunks) {
            out.set(chunk, offset);
            offset += chunk.byteLength;
        }
        return out;
    }
}
export function encodeTcpBlobRequest(hash) {
    if (hash.byteLength !== 32)
        throw new Error('TCP/FIPS blob hash must be 32 bytes');
    const request = new Uint8Array(REQUEST_BYTES);
    request.set([TCP_BLOB_MAGIC, TCP_BLOB_VERSION, GET]);
    request.set(hash, 3);
    return request;
}
export function encodeTcpBlobResponseHeader(found, size) {
    if (!Number.isInteger(size) || size < 0 || size > TCP_BLOB_MAX_BYTES) {
        throw new Error('TCP/FIPS blob response size is invalid');
    }
    const header = new Uint8Array(HEADER_BYTES);
    header.set([TCP_BLOB_MAGIC, TCP_BLOB_VERSION, found ? FOUND : 0]);
    new DataView(header.buffer).setUint32(3, size);
    return header;
}
function bytesEqual(left, right) {
    if (left.byteLength !== right.byteLength)
        return false;
    return left.every((byte, index) => byte === right[index]);
}
function sleep(ms) {
    return new Promise((resolve) => setTimeout(resolve, ms));
}
//# sourceMappingURL=tcpBlobTransport.js.map