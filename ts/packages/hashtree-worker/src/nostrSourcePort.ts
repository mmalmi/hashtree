import type {
  NostrEventSubscription, QueryEvent, QueryOptions, RuntimeSource,
} from 'nostr-pubsub';

type Request = { id: number; method: 'query' | 'publish' | 'subscribe' | 'cancel'; args?: unknown[] };
type Reply = { id: number; type: 'result' | 'event' | 'error'; value?: unknown; error?: string };

/** Serve an existing source; this bridge never creates a relay pool or a FIPS node. */
export function serveNostrSource(port: MessagePort, source: RuntimeSource): () => void {
  const operations = new Map<number, AbortController>();
  const subscriptions = new Map<number, NostrEventSubscription>();
  let closed = false;
  port.onmessage = async ({ data }: MessageEvent<Request>) => {
    if (closed || !Number.isSafeInteger(data?.id)) return;
    if (data.method === 'cancel') {
      operations.get(data.id)?.abort();
      operations.delete(data.id);
      subscriptions.get(data.id)?.close();
      subscriptions.delete(data.id);
      return;
    }
    const controller = new AbortController();
    operations.set(data.id, controller);
    const args = data.args ?? [];
    try {
      let value: unknown;
      if (data.method === 'query') {
        if (!source.query) throw new Error('Nostr source does not support queries');
        value = await source.query(args[0] as never, { ...args[1] as QueryOptions, signal: controller.signal });
      } else if (data.method === 'publish') {
        if (!source.publish) throw new Error('Nostr source does not support publishing');
        value = await source.publish(args[0] as never, args[1] as never);
      } else if (data.method === 'subscribe') {
        if (!source.subscribe) throw new Error('Nostr source does not support subscriptions');
        const subscription = await source.subscribe(args[0] as never, (event) => {
          if (!closed && !controller.signal.aborted) port.postMessage({ id: data.id, type: 'event', value: event } satisfies Reply);
        });
        if (controller.signal.aborted || closed) subscription.close();
        else subscriptions.set(data.id, subscription);
      } else return;
      if (!closed && !controller.signal.aborted) port.postMessage({ id: data.id, type: 'result', value } satisfies Reply);
    } catch (error) {
      operations.delete(data.id);
      if (!closed && !controller.signal.aborted) port.postMessage({ id: data.id, type: 'error', error: String(error) } satisfies Reply);
    } finally {
      if (data.method !== 'subscribe') operations.delete(data.id);
    }
  };
  port.start();
  return () => {
    if (closed) return;
    closed = true;
    for (const operation of operations.values()) operation.abort();
    for (const subscription of subscriptions.values()) subscription.close();
    operations.clear(); subscriptions.clear(); port.close();
  };
}

export function connectNostrSource(port: MessagePort, id: string, publishAcceptance?: RuntimeSource['publishAcceptance']): { source: RuntimeSource; close(): void } {
  let nextId = 0;
  let closed = false;
  const callbacks = new Map<number, (event: QueryEvent) => void>();
  const pending = new Map<number, { resolve(value: unknown): void; reject(error: Error): void }>();
  port.onmessage = ({ data }: MessageEvent<Reply>) => {
    if (data?.type === 'event') callbacks.get(data.id)?.(data.value as QueryEvent);
    else if (data?.type === 'result') pending.get(data.id)?.resolve(data.value);
    else if (data?.type === 'error') pending.get(data.id)?.reject(new Error(data.error));
  };
  port.start();
  const call = (method: Request['method'], args: unknown[], options: QueryOptions = {}, requestId = ++nextId): Promise<unknown> => {
    if (closed) return Promise.reject(new Error('Nostr source bridge is closed'));
    if (options.signal?.aborted) return Promise.reject(new DOMException('Nostr source request cancelled', 'AbortError'));
    return new Promise((resolve, reject) => {
      const cleanup = () => {
        pending.delete(requestId);
        if (timer) clearTimeout(timer);
        options.signal?.removeEventListener('abort', abort);
      };
      const abort = () => {
        cleanup(); port.postMessage({ id: requestId, method: 'cancel' } satisfies Request);
        reject(new DOMException('Nostr source request cancelled', 'AbortError'));
      };
      pending.set(requestId, {
        resolve: (value) => { cleanup(); resolve(value); },
        reject: (error) => { cleanup(); reject(error); },
      });
      options.signal?.addEventListener('abort', abort, { once: true });
      const timer = setTimeout(() => {
        cleanup(); port.postMessage({ id: requestId, method: 'cancel' } satisfies Request);
        reject(new DOMException('Nostr source request timed out', 'TimeoutError'));
      }, Math.max(0, (options.deadline ?? Date.now() + 30_000) - Date.now()));
      port.postMessage({ id: requestId, method, args } satisfies Request);
    });
  };
  return {
    source: {
      id,
      publishAcceptance,
      query: (filters, options = {}) => call('query', [filters, { limit: options.limit, deadline: options.deadline }], options) as ReturnType<NonNullable<RuntimeSource['query']>>,
      publish: (event, origin) => call('publish', [event, origin]) as ReturnType<NonNullable<RuntimeSource['publish']>>,
      subscribe: async (filters, handler) => {
        const subscriptionId = ++nextId;
        callbacks.set(subscriptionId, handler);
        try { await call('subscribe', [filters], {}, subscriptionId); }
        catch (error) { callbacks.delete(subscriptionId); throw error; }
        return { close: () => {
          callbacks.delete(subscriptionId);
          if (!closed) port.postMessage({ id: subscriptionId, method: 'cancel' } satisfies Request);
        } };
      },
    },
    close: () => {
      if (closed) return;
      closed = true;
      for (const requestId of new Set([...callbacks.keys(), ...pending.keys()])) port.postMessage({ id: requestId, method: 'cancel' } satisfies Request);
      for (const request of [...pending.values()]) request.reject(new Error('Nostr source bridge closed'));
      callbacks.clear(); pending.clear(); port.close();
    },
  };
}
