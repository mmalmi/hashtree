import { once } from 'node:events';
import type { BrowserContext } from '@playwright/test';
import { WebSocketServer } from 'ws';
import { createNamedGraphFixture } from 'nostr-social-graph/testing/namedGraph';

export async function serveSignedGraph(context: BrowserContext, root: string, bytes: Uint8Array, gate?: Promise<void>, publication?: Awaited<ReturnType<typeof createNamedGraphFixture>>) {
  const fixture = publication ?? await createNamedGraphFixture(root, bytes);
  const relay = new WebSocketServer({ host: '127.0.0.1', port: 0 });
  await once(relay, 'listening');
  const address = relay.address();
  if (!address || typeof address === 'string') throw new Error('Expected a loopback graph relay');
  const counts = { names: 0, directories: 0, blocks: 0, bytes: 0, aborted: 0, legacy: 0 };
  relay.on('connection', socket => socket.on('message', raw => {
    const [type, id, filter] = JSON.parse(raw.toString());
    if (type !== 'REQ') return;
    if (filter.authors?.includes(fixture.event.pubkey) && filter.kinds?.includes(30064) && filter['#d']?.includes('social-graph')) {
      counts.names++;
      socket.send(JSON.stringify(['EVENT', id, fixture.event]));
    }
    socket.send(JSON.stringify(['EOSE', id]));
  }));
  const hash = fixture.event.tags.find(tag => tag[0] === 'hash')![1];
  const directory = 'https://cdn.iris.to/' + hash + '.bin';
  context.on('requestfailed', request => { if (request.url() === directory) counts.aborted++; });
  await context.route(/^https:\/\/cdn\.iris\.to\/[0-9a-f]{64}\.bin$/, async route => {
    const url = route.request().url();
    if (url === directory) { counts.directories++; await gate; }
    const block = await fixture.readBlock(new URL(url).pathname.slice(1, -4));
    if (!block) { await route.fulfill({ status: 404 }); return; }
    counts.blocks++; counts.bytes += block.length;
    await route.fulfill({ body: Buffer.from(block), contentType: 'application/octet-stream',
      headers: { 'Access-Control-Allow-Origin': '*' } }).catch(() => {});
  });
  await context.route('https://graph-api.iris.to/**', route => { counts.legacy++; return route.abort(); });
  const snapshot = { publisher: fixture.publisher, relays: ['ws://127.0.0.1:' + address.port] };
  return { counts, workerUrl: '/worker.js?snapshot=' + encodeURIComponent(JSON.stringify(snapshot)), async close() {
    await context.unrouteAll({ behavior: 'wait' });
    for (const socket of relay.clients) socket.terminate();
    await new Promise<void>(resolve => relay.close(() => resolve()));
  } };
}
