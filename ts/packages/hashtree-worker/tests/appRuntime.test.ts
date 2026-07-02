import { afterEach, describe, expect, it, vi } from 'vitest';
import { createHtreeRuntime } from '../src/index';

type MemoryStorage = {
  getItem: (key: string) => string | null;
  setItem: (key: string, value: string) => void;
};

function installWindow(serverUrl?: string, search = '', protocol = 'htree:', hostname = 'npub1example'): void {
  vi.stubGlobal('window', {
    location: {
      protocol,
      hostname,
      search,
      origin: 'https://audio.example',
    },
    __HTREE_SERVER_URL__: serverUrl,
  });
}

function createMemoryStorage(): MemoryStorage {
  const values = new Map<string, string>();
  return {
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => {
      values.set(key, value);
    },
  };
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.resetModules();
});

describe('createHtreeRuntime', () => {
  it('builds worker config and media urls from the current runtime state', () => {
    installWindow('http://127.0.0.1:21417');
    const storage = createMemoryStorage();
    const runtime = createHtreeRuntime({
      appId: 'demo-audio',
      storage,
      clientIdFactory: () => 'client-1',
      relays: ['wss://relay.example'],
      blossomServers: [{ url: 'https://upload.example', read: false, write: true }],
    });

    expect(runtime.getWorkerConfig({
      storeName: 'demo-audio-worker',
      diagnosticsEnabled: true,
    })).toEqual({
      storeName: 'demo-audio-worker',
      diagnosticsEnabled: true,
      relays: ['ws://127.0.0.1:21417/ws'],
      blossomServers: [
        { url: 'http://127.0.0.1:21417', read: true, write: true },
        { url: 'https://upload.example', read: false, write: true },
      ],
    });

    expect(runtime.urls.media('htree://nhash1example/track.mp3', {
      clientScoped: true,
      mimeType: 'audio/mpeg',
    })).toBe('/htree/nhash1example/track.mp3?htree_c=client-1&htree_t=audio%2Fmpeg');
  });

  it('recomputes endpoints from getter functions so apps with live settings can reuse one runtime', () => {
    installWindow();
    let relays = ['wss://relay.one'];
    let blossomServers = [{ url: 'https://upload.example', read: false, write: true }];
    const runtime = createHtreeRuntime({
      relays: () => relays,
      blossomServers: () => blossomServers,
    });

    expect(runtime.endpoints.nostrRelays).toEqual(['wss://relay.one']);
    relays = ['wss://relay.two'];
    blossomServers = [{ url: 'https://cdn.example', read: true, write: false }];
    expect(runtime.endpoints).toEqual({
      htreeServerUrl: null,
      nostrRelays: ['wss://relay.two'],
      blossomServers: [{ url: 'https://cdn.example', read: true, write: false }],
    });
  });

  it('treats direct native runtimes as already ready for media port setup', async () => {
    installWindow('http://127.0.0.1:21417');
    const runtime = createHtreeRuntime();

    await expect(runtime.media.ensureReady({
      registerMediaPort: () => {
        throw new Error('should not be called');
      },
    })).resolves.toBe(true);
  });

  it('treats iris.localhost bridge runtimes as direct /htree media runtimes', async () => {
    const windowLike = {
      location: {
        protocol: 'http:',
        hostname: 'audio.npub1example.iris.localhost',
        search: '',
      },
      htree: {
        htreeBaseUrl: 'http://audio.npub1example.iris.localhost:17321',
      },
    };
    const runtime = createHtreeRuntime({ windowLike });

    expect(runtime.urls.media('htree://nhash1example/track.mp3')).toBe('/htree/nhash1example/track.mp3');
    await expect(runtime.media.ensureReady({
      registerMediaPort: () => {
        throw new Error('should not be called');
      },
    })).resolves.toBe(true);
  });
});
