import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdtemp, rm, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import * as currentTransport from '../../dist/tcpBlobTransport.js';
import { createLayout, FRESH_SEEDS, HOP_LIMIT, NODE_COUNT, PAYLOAD_BYTES, PROFILES, TIMEOUT_MS } from './layout.mjs';
import { SimulationClock } from './clock.mjs';

const { encodeTcpBlobRequest, decodeTcpBlobRequest, encodeTcpBlobResponseHeader } = currentTransport;
const BASELINE = 'ff350793daece8a7e056c135b98b05c82233e6fa';
const TRANSPORT_PATH = 'ts/packages/hashtree-fips-transport';
const TIMEOUT = new Error('simulated deadline expired');
const DEPARTED = new Error('simulated peer departed');
const CORRUPT = new Error('simulated payload failed hash verification');
const COMPLETED = new Error('simulated read completed');
const corruptBytes = Buffer.alloc(PAYLOAD_BYTES, 255);
const emptyEndpoint = {
  registerService: () => () => undefined,
  sendDatagram: () => { throw new Error('the simulator owns the carrier'); },
};
const digest = (bytes) => createHash('sha256').update(bytes).digest();
const hashKey = (hash) => Buffer.from(hash.buffer, hash.byteOffset, hash.byteLength).toString('hex');
const usageMs = (start) => {
  const usage = process.cpuUsage(start);
  return (usage.user + usage.system) / 1000;
};

async function withDeadline(clock, ms, parent, run) {
  const timer = new AbortController();
  const deadline = new AbortController();
  const signal = parent ? AbortSignal.any([parent, deadline.signal]) : deadline.signal;
  const expired = clock.sleep(ms, timer.signal).then(() => {
    deadline.abort(TIMEOUT);
    throw TIMEOUT;
  });
  try {
    return await Promise.race([run(signal), expired]);
  } finally {
    timer.abort(COMPLETED);
    deadline.abort(COMPLETED);
  }
}

function whileRequested(result, signal) {
  return new Promise((resolve, reject) => {
    const abort = () => reject(signal.reason);
    const finish = (settle) => (value) => {
      signal.removeEventListener('abort', abort);
      settle(value);
    };
    result.then(finish(resolve), finish(reject));
    if (signal.aborted) abort();
    else signal.addEventListener('abort', abort, { once: true });
  });
}

class Simulation {
  clock = new SimulationClock();
  traffic = { attempts: 0, bytes: 0, falseMisses: 0, rejectedCorrupt: 0 };
  batch = 0;
  #nodes = new Map();

  constructor(module, layout) {
    this.Transport = module.TcpBlobTransport;
    this.layout = layout;
    this.legacy = typeof this.Transport.prototype.getFromPeers !== 'function';
    this.corruptReply = module.PeerBlobHashMismatch ? new module.PeerBlobHashMismatch() : CORRUPT;
  }

  node(id) {
    if (this.#nodes.has(id)) return this.#nodes.get(id);
    const contents = new Map();
    const store = {
      get: async (hash) => contents.get(hashKey(hash)) ?? null,
      put: async (hash, data) => { contents.set(hashKey(hash), data); return true; },
    };
    const transport = new this.Transport({ endpoint: emptyEndpoint, localStore: store, timeoutMs: TIMEOUT_MS });
    // Exercise production admission, cancellation and retry decisions over
    // deterministic links. The real TCP packet exchange has its own browser and
    // Rust interop tests.
    clearInterval(transport.timer);
    assert.equal(typeof transport.fetchFromPeer, 'function');
    transport.fetchFromPeer = (peer, hash, timeout, htl, signal) => {
      const link = this.layout.edges[id].find(({ to }) => to === Number(peer));
      assert.ok(link);
      return withDeadline(this.clock, timeout, signal, (requestSignal) => (
        this.fetch(id, link, hash, htl, requestSignal)
      ));
    };
    const node = { contents, transport, forwards: new Map() };
    this.#nodes.set(id, node);
    return node;
  }

  async fetch(source, link, hash, htl, signal) {
    signal.throwIfAborted();
    const wire = encodeTcpBlobRequest(hash, htl);
    this.traffic.attempts++;
    this.traffic.bytes += wire.length;
    if (link.stalled) await this.clock.sleep(TIMEOUT_MS, signal);
    await this.clock.sleep(link.delay, signal);
    if (!this.layout.online(link.to, this.batch)) throw DEPARTED;
    const request = decodeTcpBlobRequest(wire);
    const remote = this.node(link.to);
    remote.transport.recordRequester?.(request.hash, String(source));
    const { behavior } = this.layout.peers[link.to];
    const data = behavior === 0 ? null : behavior === 1 ? corruptBytes
      : await this.lookup(link.to, request.hash, request.htl, source, signal);
    signal.throwIfAborted();
    this.traffic.bytes += encodeTcpBlobResponseHeader(data !== null, data?.length ?? 0).length
      + (data?.length ?? 0);
    await this.clock.sleep(link.delay, signal);
    if (behavior === 0) this.traffic.falseMisses++;
    if (data && !digest(data).equals(hash)) {
      this.traffic.rejectedCorrupt++;
      throw this.corruptReply;
    }
    // The earlier transport cached each valid reply inside fetchFromPeer.
    if (data && this.legacy) this.node(source).contents.set(hashKey(hash), data);
    return data;
  }

  async lookup(id, hash, htl, source, signal) {
    const node = this.node(id);
    const key = hashKey(hash);
    while (true) {
      signal.throwIfAborted();
      const cached = node.contents.get(key);
      if (cached || htl === 0) return cached ?? null;

      const active = node.forwards.get(key);
      if (active?.signal.aborted) node.forwards.delete(key);
      else if (active && active.htl > htl) return null;
      else if (active && active.htl === htl) {
        try {
          return await whileRequested(active.result, signal);
        } catch (error) {
          // Another reader's deadline belongs to that reader. A remaining
          // waiter can claim the search, as in Rust's forwarding adapter.
          if (!active.signal.aborted || signal.aborted) throw error;
          continue;
        }
      }

      const peers = this.layout.edges[id].filter(({ to }) => to !== source).map(({ to }) => String(to));
      const result = whileRequested(node.transport.get(hash, peers, htl - 1, signal), signal);
      const forward = { htl, result, signal };
      node.forwards.set(key, forward);
      try {
        return await result;
      } finally {
        if (node.forwards.get(key) === forward) node.forwards.delete(key);
      }
    }
  }

  async sample(direct) {
    const oldNow = Date.now;
    Date.now = () => this.clock.now;
    const cpuStart = process.cpuUsage();
    let reachable = 0;
    let deadlines = 0;
    let incomplete = 0;
    let misses = 0;
    let unexpectedMisses = 0;
    const distinctReachable = new Set();
    const distinctVerified = new Set();
    const latencyMs = [];
    try {
      for (this.batch = 0; this.batch < 8; this.batch++) {
        const batch = this.layout.batchReads(this.batch).map((read) => this.place(read));
        const reads = batch.map(async ({ read, data, hash }) => {
          if (read.reachable) {
            reachable++;
            distinctReachable.add(read.id);
          }
          const start = this.clock.now;
          let received;
          try {
            received = await withDeadline(this.clock, TIMEOUT_MS, null, (signal) => (
              this.lookup(read.reader, hash, direct ? 1 : HOP_LIMIT, null, signal)
            ));
          } catch (error) {
            if (error === TIMEOUT) deadlines++;
            else if (error === DEPARTED || error instanceof AggregateError) incomplete++;
            else throw error;
            return;
          }
          if (!received) {
            misses++;
            if (read.reachable) unexpectedMisses++;
            return;
          }
          assert.ok(read.reachable && data.equals(received), 'verified delivery must match a reachable holder');
          distinctVerified.add(read.id);
          latencyMs.push(this.clock.now - start);
        });
        // Register handlers before the first simulated instant to capture asynchronous failures.
        const results = Promise.allSettled(reads);
        await this.clock.drain();
        const settled = await results;
        const failed = settled.find(({ status }) => status === 'rejected');
        if (failed) throw failed.reason;
      }
      assert.equal(latencyMs.length + deadlines + incomplete + misses, 64);
      const queryCpuMs = usageMs(cpuStart);
      for (const { contents, forwards } of this.#nodes.values()) {
        assert.equal(forwards.size, 0);
        for (const [key, bytes] of contents) assert.equal(digest(bytes).toString('hex'), key);
      }
      return {
        reads: 64, reachable, distinctReachable: distinctReachable.size, verified: latencyMs.length,
        distinctVerified: distinctVerified.size, deadlines, incomplete, misses, unexpectedMisses,
        latencyMs, ...this.traffic, simulatedMs: this.clock.now - 1, queryCpuMs,
      };
    } finally {
      Date.now = oldNow;
      await Promise.all([...this.#nodes.values()].map(({ transport }) => transport.close()));
    }
  }

  place(read) {
    const data = Buffer.alloc(PAYLOAD_BYTES);
    data.writeUInt32LE(read.id);
    const hash = digest(data);
    for (const holder of read.holders) this.node(holder).contents.set(hashKey(hash), data);
    return { read, data, hash };
  }
}

async function loadTransport(dir, revision) {
  const packageDir = fileURLToPath(new URL('../..', import.meta.url));
  await symlink(join(packageDir, 'node_modules'), join(dir, 'node_modules'));
  await writeFile(join(dir, 'package.json'), '{"type":"module"}\n');
  for (const name of ['tcpBlobTransport.js', 'uploadRateLimiter.js']) {
    const input = execFileSync('git', ['show', `${revision}:${TRANSPORT_PATH}/dist/${name}`], { cwd: packageDir });
    await writeFile(join(dir, name), input);
  }
  return import(pathToFileURL(join(dir, 'tcpBlobTransport.js')).href);
}

const [version, list, revision = version === 'baseline' ? BASELINE : undefined] = process.argv.slice(2);
assert.ok(['baseline', 'candidate'].includes(version), 'usage: mesh-scale/run.mjs baseline|candidate [seed,seed,...] [commit]');
if (revision) assert.match(revision, /^[0-9a-f]{40}$/);
const seeds = list ? list.split(',').map((seed) => {
  assert.match(seed, /^\d+$/);
  return Number(seed);
}) : FRESH_SEEDS;
assert.ok(seeds.every(Number.isSafeInteger));
const scratch = await mkdtemp(join(tmpdir(), 'hashtree-ts-mesh-'));
try {
  const cpuStart = process.cpuUsage();
  const transport = revision ? await loadTransport(scratch, revision) : currentTransport;
  for (const direct of [true, false]) {
    for (const seed of seeds) {
      for (const profile of PROFILES) {
        const simulation = new Simulation(transport, createLayout(seed, profile, direct));
        const result = await simulation.sample(direct);
        console.log(JSON.stringify({ version, seed, profile, mode: direct ? 'direct' : 'mesh', nodes: NODE_COUNT, ...result }));
      }
    }
  }
  console.log(JSON.stringify({ version, processCpuMs: usageMs(cpuStart), peakRssKiB: process.resourceUsage().maxRSS }));
} finally {
  await rm(scratch, { recursive: true });
}
