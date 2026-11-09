// Derived from the accepted app fixture: authenticated FIPS, loopback-only WebSocket carrier.
import { FipsNode, generateIdentity } from '@fips/core';
import { decodeLocalKeyHint, encodeLocalKeyHintResponse, validateFipsWebSocketRecord } from '@fips/transport-websocket';
import { WebSocketServer } from 'ws';
const MAX_FRAME = 66 * 1024;
export async function seed() {
  let context, server, url, next = 0, traffic = 0, outgoing = 0, maxConcurrent = 0;
  const sockets = new Map();
  const transport = {
    type: 'websocket', mtu: 1400, maxFrameBytes: MAX_FRAME,
    async start(value) {
      context = value;
      server = new WebSocketServer({ host: '127.0.0.1', port: 0, path: '/fips', maxPayload: MAX_FRAME, perMessageDeflate: false });
      await new Promise((resolve, reject) => { server.once('listening', resolve); server.once('error', reject); });
      url = `ws://127.0.0.1:${server.address().port}/fips`;
      server.on('connection', socket => {
        if (sockets.size >= 8) { socket.close(1013, 'fixture connection limit'); return; }
        const remoteAddr = { transport: 'websocket', addr: `ws-peer://${++next}` };
        sockets.set(remoteAddr.addr, socket);
        maxConcurrent = Math.max(maxConcurrent, sockets.size);
        socket.on('message', (raw, binary) => {
          if (!binary || !context) { socket.close(1003, 'binary only'); return; }
          const wire = new Uint8Array(raw);
          traffic += wire.length;
          if (traffic > 128 * 1024 * 1024) { socket.close(1008, 'fixture traffic bound'); return; }
          const hint = decodeLocalKeyHint(wire);
          if (hint?.kind === 'request') { socket.send(encodeLocalKeyHintResponse(hint.nonce, context.localIdentity.xOnlyPubkey)); return; }
          if (hint) return;
          try { validateFipsWebSocketRecord(wire, MAX_FRAME); }
          catch { socket.close(1002, 'invalid record'); return; }
          context.onPacket({ transportType: 'websocket', remoteAddr, data: wire, receivedAtMs: Date.now() });
        });
        socket.on('close', () => { sockets.delete(remoteAddr.addr); context?.onConnectionState?.({ remoteAddr, state: 'disconnected' }); });
      });
    },
    async connect(address) { if (!sockets.has(address.addr)) throw new Error('unknown seed client'); },
    async send(address, bytes) {
      validateFipsWebSocketRecord(bytes, MAX_FRAME);
      const socket = sockets.get(address.addr);
      if (!socket || socket.readyState !== socket.OPEN || socket.bufferedAmount + bytes.length > MAX_FRAME * 16) throw new Error('seed closed or backpressure');
      socket.send(bytes);
      outgoing += bytes.length;
    },
    async stop() {
      context = undefined;
      for (const socket of sockets.values()) socket.terminate();
      sockets.clear();
      await new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve()));
    },
  };
  const node = new FipsNode({ identity: await generateIdentity(), transports: [transport], forwarding: true, routingMode: 'reply_learned' });
  await node.start();
  return { url, stop: () => node.stop(), stats: () => ({ ingressBytes: traffic, egressBytes: outgoing, connectionsAccepted: next, maxConcurrent, ingressLimitBytes: 128 * 1024 * 1024 }) };
}
