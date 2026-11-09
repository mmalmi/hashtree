// Runs only installed, qualified package bytes. No source-module fallback.
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createInterface } from 'node:readline';
import { FipsNode, identityFromSecretKey, npubFromHex, toHex } from '@fips/core';
import { WebSocketTransport } from '@fips/transport-websocket';
import { DirectFileTcp, directFileFrame, manifestDigest, prefixDigest } from '@fips/file-transfer';
import { Segment } from '@fips/tcp';
import { storage, contents, sha } from './durable.mjs';
const emit = value => process.stdout.write(`${JSON.stringify(value)}\n`);
const config = JSON.parse(await readFile(process.argv[2], 'utf8'));
assert.match(config.seed, /^ws:\/\/127\.0\.0\.1:[1-9][0-9]*\/fips$/);
const identity = await identityFromSecretKey(Buffer.from(config.testSecret, 'hex'));
const peer = toHex(identity.xOnlyPubkey);
const vectorBytes = await readFile(config.vectors);
assert.equal(sha(vectorBytes), '39254c05fb2a2dc9bc4b6dd21e775f79c25f9da089685b62254f6afdde099cf9');
const vectors = JSON.parse(vectorBytes);
const vectorFiles = vectors.manifest.files.map((row, i) => ({ filename: row.filename, sizeBytes: row.size_bytes,
  sha256: row.sha256, blob: new Blob([[Buffer.from('hello'), Buffer.alloc(0), Buffer.from([0, 1, 255])][i]]) }));
assert.equal(manifestDigest(vectorFiles), vectors.manifest.sha256);
for (const row of vectors.prefixes) assert.equal(await prefixDigest(vectorFiles, row.offset), row.sha256);
for (const row of vectors.controls) assert.equal(Buffer.from(directFileFrame(0, Buffer.from(JSON.stringify(row.control)))).toString('hex'), row.frame_hex);
const node = new FipsNode({ identity, transports: [new WebSocketTransport({ seedUrls: [config.seed] })],
  forwarding: false, routingMode: 'reply_learned' });
let gate = false, armed = false, role, sentPayload = 0, stopped = false;
const events = [];
function closeGate(reason, count) {
  if (gate) return; gate = true; emit({ event: 'gate', reason, count });
}
const disk = await storage(config.directory, length => { if (armed && role === 'receive' && length >= 65536) closeGate('saved-prefix', length); });
const endpoint = {
  registerService: (port, handler) => node.registerService(port, incoming => { if (!gate) return handler(incoming); }),
  async sendDatagram(args) {
    if (gate) return;
    if (armed && role === 'offer') {
      sentPayload += Segment.decode(args.payload).payload.length;
      if (sentPayload > 262144) { closeGate('bounded-outbound-file-stream', sentPayload); return; }
    }
    await node.sendDatagram(args);
  },
};
const engine = new DirectFileTcp({ endpoint, localPeer: peer, store: disk.store,
  createReceiveFile: (id, i, spec) => disk.sink(id, i, spec),
  onEvent: e => { const row = { event: e.type, id: e.transferId, bytes: e.transferredBytes };
    events.push(row); assert(events.length <= 4096); emit(row); },
  onError: error => { emit({ event: 'harness-error', error: error.message }); process.exitCode = 1; },
});
await node.start();
emit({ event: 'ready', peer, npub: npubFromHex(peer), vectors: { sha256: sha(vectorBytes), controls: vectors.controls.length, prefixes: vectors.prefixes.length, manifest: true } });
const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
try {
  for await (const line of lines) {
    assert(line.length <= 16384); const command = JSON.parse(line);
    if (command.op === 'stop') break;
    try {
      let value;
      if (command.op === 'offer' || command.op === 'receive') {
        role = command.op; armed = command.interrupt === true;
        const files = contents(command.size, command.salt).map(([filename, bytes]) => ({ filename,
          sizeBytes: bytes.length, sha256: sha(bytes), blob: new Blob([bytes]) }));
        if (role === 'offer') engine.registerOffer(command.id, command.token, [command.peer], files, { version: 2 });
        else await engine.receive(command.id, command.token, command.peer, files, { version: 2 });
        value = true;
      } else if (command.op === 'snapshots') value = { retained: await disk.retained(command.id),
        send: await disk.store.load(`send/${command.id}`), receive: await disk.store.load(`receive/${command.id}`), ...disk.counters() };
      else if (command.op === 'verify') value = await disk.verify(command.size, command.salt);
      else throw new Error('unknown fixture operation');
      emit({ reply: command.request, value });
    } catch (error) { emit({ reply: command.request, error: error.message }); }
  }
} finally {
  gate = true; // Lose resets deliberately; restart must recover durable application state.
  await engine.dispose(); await node.stop(); stopped = true;
  emit({ event: 'stopped', graceful: true, ...disk.counters() });
  lines.close();
}
assert(stopped);
