import { describe, expect, it } from 'vitest';
import {
  buildHtreeRequestPath,
  parseHtreeUrl,
  resolveHtreeRequestUrl,
  type HtreeRuntimeWindowLike,
} from '../src/index';

function createWindowLike(
  protocol: string,
  hostname: string,
  search = '',
  serverUrl?: string,
): HtreeRuntimeWindowLike {
  return {
    location: {
      protocol,
      hostname,
      search,
    },
    __HTREE_SERVER_URL__: serverUrl,
  };
}

describe('runtime htree urls', () => {
  it('preserves encoded mutable tree names when mapping htree urls to /htree request paths', () => {
    expect(
      parseHtreeUrl('htree://npub1example/releases%2Fnostr-vpn/v0.3.0/assets/app.zip'),
    ).toEqual({
      kind: 'mutable',
      npub: 'npub1example',
      treeName: 'releases/nostr-vpn',
      path: 'v0.3.0/assets/app.zip',
    });

    expect(
      buildHtreeRequestPath('htree://npub1example/releases%2Fnostr-vpn/v0.3.0/assets/app.zip'),
    ).toBe('/htree/npub1example/releases%2Fnostr-vpn/v0.3.0/assets/app.zip');
  });

  it('uses public gateway-style mutable urls when no direct runtime is available', () => {
    const windowLike = createWindowLike('https:', 'audio.example');

    expect(
      resolveHtreeRequestUrl('htree://npub1example/audio-catalog/root.json', {
        windowLike,
        fallbackBaseUrl: 'https://upload.example',
      }),
    ).toBe('https://upload.example/npub1example/audio-catalog/root.json');
  });

  it('uses /htree mutable urls when a native child runtime can fetch directly', () => {
    const windowLike = createWindowLike(
      'http:',
      '127.0.0.1',
      '',
      'http://127.0.0.1:21417',
    );

    expect(
      resolveHtreeRequestUrl('htree://npub1example/audio-catalog/root.json', {
        windowLike,
        fallbackBaseUrl: 'https://upload.example',
      }),
    ).toBe('http://127.0.0.1:21417/htree/npub1example/audio-catalog/root.json');
  });

  it('uses direct loopback /htree urls for iris.localhost child runtimes', () => {
    const windowLike = createWindowLike(
      'http:',
      'audio.npub1example.iris.localhost',
      '?htree_canonical=htree%3A%2F%2Fnpub1example%2Faudio%2Findex.html',
      'http://127.0.0.1:17321',
    );

    expect(
      resolveHtreeRequestUrl('htree://npub1example/audio-catalog/root.json', {
        windowLike,
        fallbackBaseUrl: 'https://upload.example',
      }),
    ).toBe('http://127.0.0.1:17321/htree/npub1example/audio-catalog/root.json');
  });

  it('uses relative /htree urls for iris.localhost bridge bases', () => {
    const windowLike = createWindowLike(
      'http:',
      'audio.npub1example.iris.localhost',
      '',
    );
    windowLike.htree = {
      htreeBaseUrl: 'http://audio.npub1example.iris.localhost:17321',
    };

    expect(
      resolveHtreeRequestUrl('htree://npub1example/audio-catalog/root.json', {
        windowLike,
        fallbackBaseUrl: 'https://upload.example',
      }),
    ).toBe('/htree/npub1example/audio-catalog/root.json');

    expect(
      resolveHtreeRequestUrl('htree://nhash1example/art/cover.jpg', {
        windowLike,
        fallbackBaseUrl: 'https://upload.example',
      }),
    ).toBe('/htree/nhash1example/art/cover.jpg');
  });

  it('uses absolute loopback /htree urls for numeric bridge bases', () => {
    const windowLike = createWindowLike(
      'http:',
      'audio.npub1example.iris.localhost',
      '?htree_canonical=htree%3A%2F%2Fnpub1example%2Faudio%2Findex.html',
    );
    windowLike.htree = {
      htreeBaseUrl: 'http://localhost:17321',
    };

    expect(
      resolveHtreeRequestUrl('htree://npub1example/audio-catalog/root.json', {
        windowLike,
        fallbackBaseUrl: 'https://upload.example',
      }),
    ).toBe('http://localhost:17321/htree/npub1example/audio-catalog/root.json');
  });

  it('keeps relative /htree urls for same-origin streaming mode', () => {
    const windowLike = createWindowLike('https:', 'audio.example');

    expect(
      resolveHtreeRequestUrl('htree://nhash1example/art/cover.jpg', {
        windowLike,
        fallbackBaseUrl: '',
      }),
    ).toBe('/htree/nhash1example/art/cover.jpg');
  });
});
