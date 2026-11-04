import fs from 'node:fs/promises';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const root = process.argv[2];
const { WebSocketServer } = await import(pathToFileURL(path.join(root, 'ws/wrapper.mjs')));
const events = new Map();
const subscriptions = new Map();
function matches(event, filter) {
  if (filter.ids && !filter.ids.some(id => event.id.startsWith(id))) return false;
  if (filter.authors && !filter.authors.some(author => event.pubkey.startsWith(author))) return false;
  if (filter.kinds && !filter.kinds.includes(event.kind)) return false;
  if (filter.since && event.created_at < filter.since) return false;
  if (filter.until && event.created_at > filter.until) return false;
  return Object.entries(filter).every(([key, values]) => !key.startsWith('#') ||
    event.tags.some(tag => tag[0] === key.slice(1) && values.includes(tag[1])));
}
const server = new WebSocketServer({ host: '127.0.0.1', port: 0, maxPayload: 1024 * 1024 });
let writes = Promise.resolve();
server.on('connection', socket => {
  subscriptions.set(socket, new Map());
  socket.on('message', bytes => {
    writes = writes.then(async () => {
      const message = JSON.parse(bytes.toString());
      if (message[0] === 'EVENT') {
        const event = message[1];
        if (!event?.id || !event.pubkey || !event.sig || !Array.isArray(event.tags) || events.size >= 100)
          throw new Error('invalid fixture event');
        events.set(event.id, event);
        await fs.writeFile(path.join(root, 'events.json'), JSON.stringify([...events.values()]));
        socket.send(JSON.stringify(['OK', event.id, true, '']));
        for (const [peer, entries] of subscriptions)
          for (const [id, filters] of entries)
            if (filters.some(filter => matches(event, filter)))
              peer.send(JSON.stringify(['EVENT', id, event]));
      } else if (message[0] === 'REQ') {
        const [, id, ...filters] = message;
        subscriptions.get(socket).set(id, filters);
        for (const event of [...events.values()].sort((a, b) => b.created_at - a.created_at))
          if (filters.some(filter => matches(event, filter)))
            socket.send(JSON.stringify(['EVENT', id, event]));
        socket.send(JSON.stringify(['EOSE', id]));
      } else if (message[0] === 'CLOSE') subscriptions.get(socket).delete(message[1]);
    }).catch(error => {
      console.error(error.message);
      if (socket.readyState === 1) socket.send(JSON.stringify(['NOTICE', 'invalid fixture request']));
    });
  });
  socket.on('close', () => subscriptions.delete(socket));
});
await new Promise((resolve, reject) => {
  server.once('listening', resolve);
  server.once('error', reject);
});
await fs.writeFile(path.join(root, 'relay.json'), JSON.stringify({ url: `ws://127.0.0.1:${server.address().port}` }));
process.on('SIGTERM', () => {
  for (const socket of server.clients) socket.terminate();
  server.close(() => process.exit(0));
});
