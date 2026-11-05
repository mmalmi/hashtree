// Observe the signed public root without the legacy CLI's fixed relay/500ms lookup.
import fs from 'node:fs/promises';
import path from 'node:path';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { pathToFileURL } from 'node:url';

const [task, inputPath, outputPath] = process.argv.slice(2);
const input = JSON.parse(await fs.readFile(inputPath, 'utf8'));
const expected = input.publication;
const { schnorr } = await import(pathToFileURL(path.join(task, 'sdk/node_modules/@noble/curves/esm/secp256k1.js')));
const relays = ['wss://upload.iris.to/ws', 'wss://cdn.iris.to/ws',
  'wss://relay.ditto.pub', 'wss://relay.damus.io', 'wss://relay.primal.net'];
const events = new Map();
const observations = [];
assert.match(expected.author, /^[0-9a-f]{64}$/);

function valid(event) {
  try {
    if (event.pubkey !== expected.author || event.kind !== 30064 ||
        !Number.isSafeInteger(event.created_at) || event.created_at < 0 ||
        event.created_at > Date.now() / 1000 + 300 || typeof event.content !== 'string' ||
        !Array.isArray(event.tags) || !event.tags.every(tag => Array.isArray(tag) &&
          tag.every(value => typeof value === 'string'))) return false;
    const names = event.tags.filter(tag => tag[0] === 'd');
    if (names.length !== 1 || names[0][1] !== 'hashtree') return false;
    const bytes = JSON.stringify([0, event.pubkey, event.created_at, event.kind, event.tags, event.content]);
    const id = createHash('sha256').update(bytes).digest('hex');
    return id === event.id && schnorr.verify(event.sig, id, event.pubkey);
  } catch { return false; }
}

// Keep subscriptions open after EOSE; slow delivery is not an absent root.
await Promise.all(relays.map(url => new Promise(resolve => {
  const socket = new WebSocket(url);
  const observation = { relay: url, state: 'connecting', verifiedEvents: 0 };
  observations.push(observation);
  const timer = setTimeout(() => { socket.close(); resolve(); }, 45_000);
  socket.addEventListener('open', () => {
    observation.state = 'subscribed';
    socket.send(JSON.stringify(['REQ', 'source-root', {
      kinds: [30064], authors: [expected.author], '#d': ['hashtree'], limit: 32,
    }]));
  });
  socket.addEventListener('error', () => { observation.state = 'error'; });
  socket.addEventListener('message', message => {
    if (String(message.data).length > 256 * 1024) return;
    try {
      const [type, id, event] = JSON.parse(message.data);
      if (id !== 'source-root') return;
      if (type === 'EOSE') { observation.state = 'live'; return; }
      if (type !== 'EVENT' || !valid(event)) return;
      assert(events.size < 128 || events.has(event.id), 'event budget');
      events.set(event.id, event);
      observation.verifiedEvents++;
    } catch (error) { observation.lastError = error.message; }
  });
  socket.addEventListener('close', () => {
    clearTimeout(timer);
    if (observation.state !== 'error') observation.state = 'closed';
    resolve();
  });
})));

const latest = [...events.values()].sort((a, b) => b.created_at - a.created_at || a.id.localeCompare(b.id))[0];
assert(latest, 'No verified public root observed; failed/slow relays are not absence evidence');
for (const name of ['hash', 'key']) {
  const tags = latest.tags.filter(tag => tag[0] === name);
  assert.equal(tags.length, 1);
  assert.equal(tags[0][1], expected[name], `Latest signed ${name} differs from qualified root`);
}
assert(latest.content === '' || latest.content === expected.hash);
await fs.writeFile(outputPath, JSON.stringify({ event: latest, signatureVerified: true,
  observedWindowSeconds: 45, observations }, null, 2) + '\n');
console.log(JSON.stringify({ signedRootMatchesQualifiedRoot: true, eventId: latest.id,
  verifiedEvents: events.size }));
