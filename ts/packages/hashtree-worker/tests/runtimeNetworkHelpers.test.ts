import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  resolveRuntimeEndpoints,
} from '../src/index';

function installWindow(serverUrl?: string, search = ''): void {
  vi.stubGlobal('window', {
    location: {
      protocol: 'htree:',
      hostname: 'npub1example',
      search,
    },
    __HTREE_SERVER_URL__: serverUrl,
  });
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.resetModules();
});

describe('runtime network helpers', () => {
  it('routes nostr relay traffic through the embedded daemon when available', () => {
    installWindow('http://127.0.0.1:21417');

    expect(resolveRuntimeEndpoints({
      relays: ['wss://relay.example', 'wss://relay.example/'],
    }).nostrRelays).toEqual(['ws://127.0.0.1:21417/ws']);
  });

  it('prepends the embedded daemon blossom server and deduplicates matching entries', () => {
    installWindow(undefined, '?htree_server=http%3A%2F%2F127.0.0.1%3A21417');

    expect(resolveRuntimeEndpoints({
      blossomServers: [
        { url: 'http://127.0.0.1:21417/', read: true, write: false },
        { url: 'https://upload.example', read: false, write: true },
      ],
    }).blossomServers).toEqual([
      { url: 'http://127.0.0.1:21417', read: true, write: true },
      { url: 'https://upload.example', read: false, write: true },
    ]);
  });

  it('preserves batch-first read preferences through blossom endpoint normalization', () => {
    installWindow();

    expect(resolveRuntimeEndpoints({
      blossomServers: [
        { url: 'https://cdn.example/', read: true, write: false, preferBatchReads: true },
        { url: 'https://cdn.example', read: false, write: true },
      ],
    }).blossomServers).toEqual([
      { url: 'https://cdn.example', read: true, write: true, preferBatchReads: true },
    ]);
  });

  it('keeps normalized upstream relays when no embedded daemon runtime exists', () => {
    installWindow();

    expect(resolveRuntimeEndpoints({
      relays: ['wss://relay.example/', 'wss://relay.example', 'wss://relay.two'],
    })).toEqual({
      htreeServerUrl: null,
      nostrRelays: ['wss://relay.example', 'wss://relay.two'],
      blossomServers: [],
    });
  });

  it('returns the active htree, nostr, and blossom endpoints together', () => {
    installWindow('http://127.0.0.1:21417');

    expect(resolveRuntimeEndpoints({
      relays: ['wss://relay.example'],
      blossomServers: [{ url: 'https://upload.example', read: false, write: true }],
    })).toEqual({
      htreeServerUrl: 'http://127.0.0.1:21417',
      nostrRelays: ['ws://127.0.0.1:21417/ws'],
      blossomServers: [
        { url: 'http://127.0.0.1:21417', read: true, write: true },
        { url: 'https://upload.example', read: false, write: true },
      ],
    });
  });
});
