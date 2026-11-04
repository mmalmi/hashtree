// Capture the public signed envelope. Schnorr and decrypted-ref verification is
// performed independently by the existing site verifier, not inferred here.
import assert from 'node:assert/strict';
import { createHash, randomBytes } from 'node:crypto';
import { writeFile } from 'node:fs/promises';

const [author, repository, output] = process.argv.slice(2);
assert.match(author ?? '', /^[0-9a-f]{64}$/);
assert.match(repository ?? '', /^hashtree-installer-fixture-[0-9a-f]{16}$/);
assert(output && typeof WebSocket === 'function');
const relays = ['wss://upload.iris.to/ws', 'wss://cdn.iris.to/ws',
  'wss://temp.iris.to', 'wss://nos.lol', 'wss://relay.snort.social'];
const sessions = [];

function observe(relay) {
  return new Promise((resolve, reject) => {
    const id = randomBytes(8).toString('hex');
    const socket = new WebSocket(relay);
    const timer = setTimeout(() => reject(new Error('Public root observation timed out')), 45000);
    sessions.push({ socket, id, timer });
    socket.addEventListener('open', () => {
      console.log(JSON.stringify({relay, phase: 'open'}));
      socket.send(JSON.stringify(['REQ', id, { kinds: [30064], authors: [author], '#d': [repository], limit: 10 }]));
    });
    socket.addEventListener('error', () => { console.log(JSON.stringify({relay, phase: 'error'})); reject(new Error('Public relay transport failed')); });
    socket.addEventListener('close', () => reject(new Error('Public relay closed before root observation')));
    socket.addEventListener('message', ({ data }) => {
      // EOSE is not evidence of absence: keep the subscription open for a live root.
      if (typeof data !== 'string' || data.length > 65536) return;
      try {
        const message = JSON.parse(data);
        if (message[0] === 'EOSE' && message[1] === id) console.log(JSON.stringify({relay, phase: 'eose'}));
        if (message[0] !== 'EVENT' || message[1] !== id) return;
        const event = message[2];
        if (event.kind !== 30064 || event.pubkey !== author || !Array.isArray(event.tags)
            || !Number.isSafeInteger(event.created_at) || typeof event.content !== 'string') return;
        if (!event.tags.every(t => Array.isArray(t) && t.every(x => typeof x === 'string'))) return;
        const tag = name => {
          const matches = event.tags.filter(t => t[0] === name);
          return matches.length === 1 ? matches[0][1] : undefined;
        };
        if (tag('d') !== repository || !/^[0-9a-f]{64}$/.test(tag('hash') ?? '')
            || !/^01[0-9a-f]{64}$/.test(tag('key') ?? '') || !/^[0-9a-f]{128}$/.test(event.sig ?? '')) return;
        const canonical = JSON.stringify([0, author, event.created_at, event.kind, event.tags, event.content]);
        if (event.id !== createHash('sha256').update(canonical).digest('hex')) return;
        console.log(JSON.stringify({relay, phase: 'accepted'}));
        resolve(event);
      } catch { /* Malformed events are not accepted, and do not terminate the subscription. */ }
    });
  });
}

try {
  const event = await Promise.any(relays.map(observe));
  await writeFile(output, JSON.stringify(event, null, 2) + '\n', { mode: 0o600, flag: 'wx' });
  console.log(JSON.stringify({ eventId: event.id, signatureVerification: 'pending independent site verifier' }));
} catch {
  console.error('No matching public root was observed within the bounded relay window; this is not proof of absence');
  process.exitCode = 1;
} finally {
  for (const { socket, id, timer } of sessions) {
    clearTimeout(timer);
    if (socket.readyState === WebSocket.OPEN) socket.send(JSON.stringify(['CLOSE', id]));
    socket.close();
  }
}
