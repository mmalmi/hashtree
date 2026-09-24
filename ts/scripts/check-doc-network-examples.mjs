import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { once } from 'node:events';
import { WebSocketServer } from 'ws';
import { SimplePool, finalizeEvent, generateSecretKey, getPublicKey, matchFilters, verifyEvent } from 'nostr-tools';
import { HashTree, MemoryStore, nhashDecode, sha256, toHex } from '../packages/hashtree/dist/index.js';

// Exercise the documented adapters over real HTTP and WebSocket connections.
// These loopback servers speak the relevant Blossom and Nostr protocol subset.
export async function checkDocNetworkExamples({ uploadFile, downloadFile, publishAndFollowRoot }) {
  const blocks = new Map();
  const events = new Map();
  const subscriptions = new Map();
  const failures = [];
  const server = createServer(async (request, response) => {
    try {
      if (request.method === 'PUT' && request.url === '/upload') {
        const auth = JSON.parse(Buffer.from(request.headers.authorization.slice(6), 'base64').toString());
        assert.ok(verifyEvent(auth));
        assert.equal(auth.kind, 24242);
        assert.ok(auth.tags.some(([name, value]) => name === 't' && value === 'upload'));
        const chunks = [];
        for await (const chunk of request) chunks.push(chunk);
        const data = Buffer.concat(chunks);
        const hash = toHex(await sha256(data));
        assert.ok(auth.tags.some(([name, value]) => name === 'x' && value === hash));
        assert.equal(request.headers['x-sha-256'], hash);
        blocks.set(hash, data);
        response.writeHead(201, { 'Content-Type': 'application/json' });
        response.end(JSON.stringify({ sha256: hash, size: data.length }));
        return;
      }
      const hash = /^\/([0-9a-f]{64})(?:\.bin)?$/.exec(request.url)?.[1];
      const data = blocks.get(hash);
      response.writeHead(data ? 200 : 404);
      response.end(request.method === 'HEAD' ? undefined : data);
    } catch (error) {
      failures.push(error);
      response.writeHead(500);
      response.end();
    }
  });
  const relay = new WebSocketServer({ server });
  relay.on('connection', (socket) => {
    const filtersById = new Map();
    subscriptions.set(socket, filtersById);
    socket.on('message', (bytes) => {
      try {
        const [type, idOrEvent, ...filters] = JSON.parse(bytes.toString());
        if (type === 'REQ') {
          filtersById.set(idOrEvent, filters);
          for (const event of events.values()) {
            if (matchFilters(filters, event)) socket.send(JSON.stringify(['EVENT', idOrEvent, event]));
          }
          socket.send(JSON.stringify(['EOSE', idOrEvent]));
        } else if (type === 'CLOSE') {
          filtersById.delete(idOrEvent);
        } else if (type === 'EVENT') {
          const event = idOrEvent;
          assert.ok(verifyEvent(event));
          events.set(event.id, event);
          for (const [peer, entries] of subscriptions) {
            for (const [id, filters] of entries) {
              if (matchFilters(filters, event)) peer.send(JSON.stringify(['EVENT', id, event]));
            }
          }
          socket.send(JSON.stringify(['OK', event.id, true, '']));
        }
      } catch (error) {
        failures.push(error);
        socket.close();
      }
    });
    socket.on('close', () => subscriptions.delete(socket));
  });
  const peer = new SimplePool();
  let stop;
  try {
    server.listen(0, '127.0.0.1');
    await once(server, 'listening');
    const host = `127.0.0.1:${server.address().port}`;
    const secret = generateSecretKey();
    const signEvent = async (event) => finalizeEvent(event, secret);
    const data = new TextEncoder().encode('Hello, remote storage! '.repeat(110_000));
    const identifier = await uploadFile(`http://${host}`, signEvent, data);
    assert.ok(blocks.size >= 3, 'The upload must include file chunks and their root');
    assert.deepEqual(await downloadFile(`http://${host}`, identifier), data);
    await assert.rejects(downloadFile(`http://${host}`, identifier, 1), /exceed|limit/i);

    const root = nhashDecode(identifier);
    const pubkey = getPublicKey(secret);
    const relays = [`ws://${host}`];
    const updates = [];
    stop = await publishAndFollowRoot(relays, 'myfiles', pubkey, signEvent, root,
      (cid) => updates.push(cid && toHex(cid.hash)));
    assert.ok(updates.includes(toHex(root.hash)));
    const published = [...events.values()].find((event) => event.kind === 30064);
    assert.ok(published, 'The root must actually reach the relay');
    assert.equal(published.tags.find(([name]) => name === 'key')?.[1], toHex(root.key));
    const fetched = await peer.get(relays, { authors: [pubkey], kinds: [30064], '#d': ['myfiles'] });
    assert.equal(fetched?.id, published.id, 'A separate client can discover the root');

    // A later update must arrive after EOSE; an optimistic local callback alone
    // does not establish that the documented subscription remains live.
    const next = await new HashTree({ store: new MemoryStore() }).putFile(new Uint8Array([1, 2, 3]));
    const nextEvent = finalizeEvent({
      kind: 30064, created_at: published.created_at + 1, content: '',
      tags: [['d', 'myfiles'], ['l', 'hashtree'], ['hash', toHex(next.cid.hash)], ['key', toHex(next.cid.key)]],
    }, secret);
    await Promise.any(peer.publish(relays, nextEvent));
    await waitFor(() => updates.includes(toHex(next.cid.hash)));
    assert.deepEqual(failures, [], 'Loopback servers rejected a documented request');
  } finally {
    const closed = [...relay.clients].map((socket) => once(socket, 'close'));
    stop?.();
    peer.destroy();
    // Complete the client-initiated close handshake; terminating a connection
    // mid-close can trigger the client's reconnect policy instead.
    const fallback = setTimeout(() => {
      for (const socket of relay.clients) socket.terminate();
    }, 1_000);
    await Promise.all(closed);
    clearTimeout(fallback);
    await new Promise((resolve) => relay.close(resolve));
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
  }
}

async function waitFor(predicate) {
  const deadline = Date.now() + 5_000;
  while (!predicate()) {
    if (Date.now() >= deadline) throw new Error('Live root update did not arrive');
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}
