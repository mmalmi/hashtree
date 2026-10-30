import 'fake-indexeddb/auto';
import { createServer } from 'node:http';
import { once } from 'node:events';
import Dexie from 'dexie';
import { DexieStore } from '@hashtree/dexie';
import { finalizeEvent } from 'nostr-tools/pure';
import { matchFilters } from 'nostr-tools/filter';
import { Reconciliation } from 'nostr-pubsub-reconcile';
import { WebSocket, WebSocketServer } from 'ws';
import { expect, it, vi } from 'vitest';
import { closeNostrRuntime, ingestEvents, initNostrRuntime, query, setOnEvent, subscribe } from '../src/relay/nostrRuntime.js';
import { createWorkerEventStore } from '../src/relay/eventIndex.js';

it('reconciles against the persisted worker cache and downloads only missing IDs without publishing', async () => {
  const now = Math.floor(Date.now() / 1000), key = new Uint8Array(32).fill(9);
  const make = (id: string) => finalizeEvent({kind:30402, created_at:now - 10, tags:[['d',id]], content:id}, key);
  const cached = make('cached'), missing = make('missing'), events = [cached, missing];
  const frames: any[][] = [], sessions = new Map<string, Reconciliation>();
  const http = createServer((_request, response) => {
    response.setHeader('Content-Type', 'application/nostr+json');
    response.end(JSON.stringify({supported_nips:[77]}));
  });
  const relay = new WebSocketServer({server:http});
  relay.on('connection', socket => socket.on('message', async data => {
    const frame = JSON.parse(data.toString()); frames.push(frame);
    const [type,id] = frame;
    if (type === 'REQ') {
      // Ordinary history deliberately misses one record.
      const candidates = frame.slice(2).some((filter: any) => filter.ids) ? events : [];
      for (const event of candidates) if (matchFilters(frame.slice(2), event)) socket.send(JSON.stringify(['EVENT',id,event]));
      socket.send(JSON.stringify(['EOSE',id]));
    } else if (type === 'NEG-OPEN' || type === 'NEG-MSG') {
      if (type === 'NEG-OPEN') {
        const filter = frame[2];
        sessions.set(id, new Reconciliation(events.filter(event => matchFilters([filter],event)).map(event => ({
          timestamp:BigInt(event.created_at), id:Uint8Array.from(Buffer.from(event.id,'hex')),
        })), {since:BigInt(filter.since),until:BigInt(filter.until)}, {maxFrameBytes:4096}));
      }
      const response = await sessions.get(id)!.respond(Uint8Array.from(Buffer.from(frame[type === 'NEG-OPEN' ? 3 : 2],'hex')));
      if (socket.readyState === WebSocket.OPEN) socket.send(JSON.stringify(['NEG-MSG',id,Buffer.from(response).toString('hex')]));
    }
  }));
  http.listen(0,'127.0.0.1'); await once(http,'listening');
  const address = http.address(); if (!address || typeof address === 'string') throw new Error('Missing test relay port');
  const name = `worker-negentropy-${crypto.randomUUID()}`;
  let blocks = new DexieStore(name);
  vi.stubGlobal('WebSocket',WebSocket);
  try {
    await initNostrRuntime([], {store:blocks,storeName:name});
    await ingestEvents([cached]);
    const stored = createWorkerEventStore(blocks,name);
    expect(await stored.listPending()).toEqual([]);
    await stored.close(); await closeNostrRuntime(); blocks.close();
    blocks = new DexieStore(name);
    await initNostrRuntime([`ws://127.0.0.1:${address.port}/`], {store:blocks,storeName:name,reconciliation:{timeoutMs:1500}});
    const received: string[] = [];
    setOnEvent((_id,event) => {received.push(event.id);});
    subscribe('public-listings',[{kinds:[30402],limit:1}]);
    await vi.waitFor(() => expect(received).toContain(missing.id), {timeout:2500});
    expect(received[0]).toBe(cached.id);
    const requested = frames.filter(frame => frame[0] === 'REQ').flatMap(frame => frame.slice(2).flatMap(filter => filter.ids ?? []));
    expect(requested).toEqual([missing.id]);
    expect(frames.some(frame => frame[0] === 'EVENT')).toBe(false);
    expect((await query([{kinds:[30402]}], {cache:'cache-only'})).events.map(event=>event.id).sort()).toEqual(events.map(event=>event.id).sort());
    await expect(ingestEvents([{...cached,content:'forged'}])).rejects.toThrow();
    await expect(ingestEvents(Array(129).fill(cached))).rejects.toThrow(/bounds/);
  } finally {
    setOnEvent(null); await closeNostrRuntime(); blocks.close(); vi.unstubAllGlobals();
    for (const socket of relay.clients) socket.terminate();
    http.closeAllConnections(); await new Promise<void>(resolve=>relay.close(()=>resolve()));
    await new Promise<void>(resolve=>http.close(()=>resolve()));
    await Dexie.delete(name); await Dexie.delete(`${name}-nostr-index`);
  }
});
