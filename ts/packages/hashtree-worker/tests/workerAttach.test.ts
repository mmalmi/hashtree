import 'fake-indexeddb/auto';
import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  attachHashtreeWorker,
  type HashtreeWorkerMessageEndpoint,
  type HashtreeWorkerRuntime,
} from '../src/worker';

class FakeWorkerEndpoint implements HashtreeWorkerMessageEndpoint {
  readonly responses: unknown[] = [];
  started = 0;
  private readonly listeners = new Set<EventListener>();

  addEventListener(_type: 'message', listener: EventListenerOrEventListenerObject): void {
    if (typeof listener === 'function') {
      this.listeners.add(listener);
      return;
    }
    this.listeners.add(listener.handleEvent.bind(listener) as EventListener);
  }

  removeEventListener(_type: 'message', listener: EventListenerOrEventListenerObject): void {
    if (typeof listener === 'function') {
      this.listeners.delete(listener);
      return;
    }
    this.listeners.delete(listener.handleEvent.bind(listener) as EventListener);
  }

  postMessage(message: unknown): void {
    this.responses.push(message);
  }

  start(): void {
    this.started += 1;
  }

  dispatch(data: unknown): void {
    const event = { data } as MessageEvent<unknown>;
    for (const listener of this.listeners) {
      listener(event as unknown as Event);
    }
  }
}

async function flushMicrotasks(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe('attachHashtreeWorker', () => {
  it('handles protocol messages without owning the whole worker global', async () => {
    const endpoint = new FakeWorkerEndpoint();
    const detach = attachHashtreeWorker(endpoint);

    expect(endpoint.started).toBe(1);

    endpoint.dispatch({ custom: true });
    await flushMicrotasks();
    expect(endpoint.responses).toEqual([]);

    endpoint.dispatch({ type: 'close', id: 'req-1' });
    await vi.waitFor(() => {
      expect(endpoint.responses).toContainEqual({ type: 'void', id: 'req-1' });
    });

    detach();
    endpoint.responses.length = 0;
    endpoint.dispatch({ type: 'close', id: 'req-2' });
    await flushMicrotasks();
    expect(endpoint.responses).toEqual([]);
  });

  it('lets an application extension use the initialized routed store', async () => {
    const endpoint = new FakeWorkerEndpoint();
    let runtimeSeen: HashtreeWorkerRuntime | null = null;
    const detach = attachHashtreeWorker(endpoint, {
      handleExtensionRequest(request, runtime) {
        if (
          !request
          || typeof request !== 'object'
          || (request as { type?: unknown }).type !== 'readSocialData'
        ) {
          return false;
        }
        runtimeSeen = runtime;
        runtime.postMessage({ type: 'socialData', initialized: Boolean(runtime.store && runtime.tree) });
        return true;
      },
    });

    endpoint.dispatch({
      type: 'init',
      id: 'init-1',
      config: { storeName: `worker-extension-${Date.now()}`, blossomServers: [] },
    });
    await vi.waitFor(() => {
      expect(endpoint.responses).toContainEqual({ type: 'ready', id: 'init-1' });
    });

    endpoint.dispatch({ type: 'readSocialData' });
    await vi.waitFor(() => {
      expect(endpoint.responses).toContainEqual({ type: 'socialData', initialized: true });
    });
    expect(runtimeSeen?.store).not.toBeNull();
    expect(runtimeSeen?.tree).not.toBeNull();

    detach();
  });
});
