import { expect, it } from 'vitest';
import { InMemoryEventBus } from 'nostr-pubsub';
import { finalizeEvent } from 'nostr-tools/pure';
import { connectNostrSource, serveNostrSource } from '../src/nostrSourcePort.js';

const origin = { id: 'test', kind: 'local-index' as const };
const signed = finalizeEvent({ kind: 1, created_at: 10, content: 'shared node', tags: [] }, new Uint8Array(32).fill(2));

it('shares verified events, query completion and publication receipts across a real message channel', async () => {
  const bus = new InMemoryEventBus();
  const channel = new MessageChannel();
  const serve = serveNostrSource(channel.port1, {
    id: 'existing-node',
    publish: bus.publish.bind(bus), query: bus.query.bind(bus), subscribe: bus.subscribe.bind(bus),
  });
  const bridge = connectNostrSource(channel.port2, 'existing-node', 'queued');
  try {
    expect(bridge.source.publishAcceptance).toBe('queued');
    let received = 0;
    let delivered!: () => void;
    const delivery = new Promise<void>((resolve) => { delivered = resolve; });
    const subscription = await bridge.source.subscribe!([{ kinds: [1] }], ({ event }) => {
      expect(event.id).toBe(signed.id); received += 1; delivered();
    });
    const receipt = await bridge.source.publish!(signed, origin);
    expect(receipt.accepted).toBe(true);
    await delivery;
    const result = await bridge.source.query!([{ kinds: [1] }]);
    expect(result.events.map(({ event }) => event.id)).toEqual([signed.id]);
    subscription.close();
    await bridge.source.query!([{}]); // Drain the ordered channel after CLOSE.
    await bus.publish(signed, origin);
    expect(received).toBe(1);
  } finally { bridge.close(); serve(); }
});

it('forwards query cancellation to the source and releases pending requests on close', async () => {
  const channel = new MessageChannel();
  let observedAbort!: () => void;
  const aborted = new Promise<void>((resolve) => { observedAbort = resolve; });
  const serve = serveNostrSource(channel.port1, {
    id: 'slow',
    query: async (_filters, options) => new Promise((_resolve, reject) => {
      options?.signal?.addEventListener('abort', () => { observedAbort(); reject(new Error('aborted')); });
    }),
  });
  const bridge = connectNostrSource(channel.port2, 'slow');
  const controller = new AbortController();
  const pending = bridge.source.query!([{}], { signal: controller.signal });
  controller.abort();
  await expect(pending).rejects.toThrow('cancelled');
  await aborted;
  bridge.close(); serve();
});
