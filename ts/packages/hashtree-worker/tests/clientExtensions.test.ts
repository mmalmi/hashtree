import { describe, expect, it } from 'vitest';
import { HashtreeWorkerClient } from '../src/client.js';
import type { WorkerRequest, WorkerResponse } from '../src/protocol.js';

class ExtensionWorker {
  static instances: ExtensionWorker[] = [];

  onmessage: ((event: MessageEvent<unknown>) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;
  onmessageerror: ((event: MessageEvent<unknown>) => void) | null = null;
  readonly requests: unknown[] = [];

  constructor() {
    ExtensionWorker.instances.push(this);
  }

  postMessage(message: WorkerRequest | { type: 'socialQuery' }, _transfer?: Transferable[]): void {
    this.requests.push(message);
    if (message.type === 'init') {
      this.emit({ type: 'ready', id: message.id } satisfies WorkerResponse);
      return;
    }
    if (message.type === 'socialQuery') {
      this.emit({ type: 'socialDelta', notes: ['one'] });
      return;
    }
    if (message.type === 'close') {
      this.emit({ type: 'void', id: message.id } satisfies WorkerResponse);
    }
  }

  terminate(): void {
    // no-op
  }

  fail(message = 'Extension worker crashed'): void {
    this.onerror?.({ message } as ErrorEvent);
  }

  private emit(message: unknown): void {
    this.onmessage?.({ data: message } as MessageEvent<unknown>);
  }
}

describe('HashtreeWorkerClient extensions', () => {
  it('initializes before posting extension messages and delivers extension responses', async () => {
    const client = new HashtreeWorkerClient(ExtensionWorker as unknown as new () => Worker);
    const messages: unknown[] = [];
    const unsubscribe = client.onExtensionMessage((message) => {
      messages.push(message);
    });

    await client.postExtensionMessage({ type: 'socialQuery' });

    expect(messages).toEqual([{ type: 'socialDelta', notes: ['one'] }]);
    unsubscribe();
    await client.close();
  });

  it('notifies extension callers when the worker fails and respawns on the next request', async () => {
    ExtensionWorker.instances = [];
    const client = new HashtreeWorkerClient(ExtensionWorker as unknown as new () => Worker);
    const failures: Error[] = [];
    client.onExtensionError((error) => failures.push(error));

    await client.postExtensionMessage({ type: 'waitingSocialQuery' });
    ExtensionWorker.instances[0]!.fail('Worker process exited');

    expect(failures.map((error) => error.message)).toEqual(['Worker process exited']);
    await client.postExtensionMessage({ type: 'socialQuery' });
    expect(ExtensionWorker.instances).toHaveLength(2);
    await client.close();
  });

  it('notifies extension callers immediately when the client closes', async () => {
    const client = new HashtreeWorkerClient(ExtensionWorker as unknown as new () => Worker);
    const failure = new Promise<Error>((resolve) => client.onExtensionError(resolve));

    await client.init();
    const closing = client.close();

    await expect(failure).resolves.toMatchObject({ message: 'Worker closed' });
    await closing;
  });
});
