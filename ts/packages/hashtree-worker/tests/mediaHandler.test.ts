import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { HashTree, MemoryStore, nhashEncode, toHex, type CID, type HashTree } from '@hashtree/core';
import { nip19 } from 'nostr-tools';
import { storeTreeEventSnapshot } from '@hashtree/nostr';
import { __test__, initMediaHandler, registerMediaPort } from '../src/relay/mediaHandler';

const ROOT: CID = { hash: Uint8Array.from({ length: 32 }, (_, i) => i), key: undefined };
const CHILD_DIR: CID = { hash: Uint8Array.from({ length: 32 }, (_, i) => i + 1), key: undefined };
const ROOT_THUMB: CID = { hash: Uint8Array.from({ length: 32 }, (_, i) => i + 2), key: undefined };
const CHILD_THUMB: CID = { hash: Uint8Array.from({ length: 32 }, (_, i) => i + 3), key: undefined };
const KEYED_THUMB: CID = {
  hash: Uint8Array.from({ length: 32 }, (_, i) => i + 4),
  key: Uint8Array.from({ length: 32 }, (_, i) => 255 - i),
};

const resolvePath = vi.fn();
const listDirectory = vi.fn();
const getBlob = vi.fn();
const getTreeNode = vi.fn();
const readFile = vi.fn();
const readFileRange = vi.fn();
const readFileStream = vi.fn();

function sameHash(a: Uint8Array | undefined, b: Uint8Array | undefined): boolean {
  if (!a || !b || a.length !== b.length) return false;
  return a.every((value, index) => value === b[index]);
}

function makeTree(): HashTree {
  return {
    resolvePath,
    listDirectory,
    getBlob,
    getTreeNode,
    readFile,
    readFileRange,
    readFileStream,
  } as unknown as HashTree;
}

describe('mediaHandler thumbnail aliases', () => {
  beforeEach(() => {
    resolvePath.mockReset();
    listDirectory.mockReset();
    getBlob.mockReset();
    getTreeNode.mockReset();
    readFile.mockReset();
    readFileRange.mockReset();
    readFileStream.mockReset();
    initMediaHandler(makeTree());
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it('resolves a root thumbnail alias for immutable nhash requests', async () => {
    resolvePath.mockResolvedValue(null);
    listDirectory.mockResolvedValue([{ name: 'thumbnail.jpg', cid: ROOT_THUMB, size: 123 }]);

    await expect(
      __test__.resolveCidWithinRoot(ROOT, 'thumbnail', { allowSingleSegmentRootFallback: true })
    ).resolves.toBe(ROOT_THUMB);
    expect(resolvePath).not.toHaveBeenCalled();
  });

  it('accepts generic image filenames for thumbnail aliases', async () => {
    resolvePath.mockResolvedValue(null);
    listDirectory.mockResolvedValue([{ name: 'cover.jpeg', cid: ROOT_THUMB, size: 123 }]);

    await expect(
      __test__.resolveCidWithinRoot(ROOT, 'thumbnail', { allowSingleSegmentRootFallback: true })
    ).resolves.toBe(ROOT_THUMB);
    await expect(__test__.normalizeAliasPath(ROOT, 'thumbnail')).resolves.toBe('cover.jpeg');
    expect(resolvePath).not.toHaveBeenCalled();
  });

  it('resolves nested thumbnail aliases before looking up the file cid', async () => {
    resolvePath.mockResolvedValue(null);
    listDirectory.mockImplementation(async (cid: CID) => {
      if (cid === ROOT) {
        return [{ name: 'video_123', cid: CHILD_DIR, size: 0 }];
      }
      if (cid === CHILD_DIR) {
        return [{ name: 'thumbnail.jpg', cid: CHILD_THUMB, size: 1 }];
      }
      return [];
    });

    await expect(
      __test__.resolveCidWithinRoot(ROOT, 'video_123/thumbnail', { allowSingleSegmentRootFallback: true })
    ).resolves.toBe(CHILD_THUMB);
    expect(resolvePath).not.toHaveBeenCalled();
  });

  it('resolves nested direct file paths from cached directory listings instead of tree.resolvePath', async () => {
    resolvePath.mockResolvedValue(null);
    listDirectory.mockImplementation(async (cid: CID) => {
      if (cid === ROOT) {
        return [{ name: 'video_123', cid: CHILD_DIR, size: 0 }];
      }
      if (cid === CHILD_DIR) {
        return [{ name: 'thumbnail.jpg', cid: CHILD_THUMB, size: 321 }];
      }
      return [];
    });

    await expect(
      __test__.resolveCidWithinRoot(ROOT, 'video_123/thumbnail.jpg', { allowSingleSegmentRootFallback: false })
    ).resolves.toBe(CHILD_THUMB);
    expect(resolvePath).not.toHaveBeenCalled();
  });

  it('resolves a video alias to the root playable file from directory listings', async () => {
    resolvePath.mockResolvedValue(null);
    listDirectory.mockResolvedValue([{ name: 'clip.mkv', cid: ROOT_THUMB, size: 321 }]);

    await expect(
      __test__.resolveCidWithinRoot(ROOT, 'video', { allowSingleSegmentRootFallback: true })
    ).resolves.toBe(ROOT_THUMB);
    await expect(__test__.normalizeAliasPath(ROOT, 'video')).resolves.toBe('clip.mkv');
    expect(resolvePath).not.toHaveBeenCalled();
  });

  it('resolves a nested video alias before looking up the file cid', async () => {
    resolvePath.mockResolvedValue(null);
    listDirectory.mockImplementation(async (cid: CID) => {
      if (cid === ROOT) {
        return [{ name: 'video_123', cid: CHILD_DIR, size: 0 }];
      }
      if (cid === CHILD_DIR) {
        return [{ name: 'movie.mov', cid: CHILD_THUMB, size: 654 }];
      }
      return [];
    });

    await expect(
      __test__.resolveCidWithinRoot(ROOT, 'video_123/video', { allowSingleSegmentRootFallback: false })
    ).resolves.toBe(CHILD_THUMB);
    await expect(__test__.normalizeAliasPath(ROOT, 'video_123/video')).resolves.toBe('video_123/movie.mov');
    expect(resolvePath).not.toHaveBeenCalled();
  });

  it('treats immutable single-segment paths as direct file cids when the root is not a directory', async () => {
    vi.useFakeTimers();
    listDirectory.mockImplementation(() => new Promise(() => {}));

    const result = __test__.resolveCidWithinRoot(ROOT, 'video.mp4', {
      allowSingleSegmentRootFallback: true,
    });

    await vi.advanceTimersByTimeAsync(1100);

    await expect(result).resolves.toBe(ROOT);
    expect(resolvePath).not.toHaveBeenCalled();
  });

  it('does not treat a thumbnail alias as a direct file cid when the root is not a directory', async () => {
    vi.useFakeTimers();
    listDirectory.mockImplementation(() => new Promise(() => {}));

    const result = __test__.resolveCidWithinRoot(ROOT, 'thumbnail', {
      allowSingleSegmentRootFallback: true,
    });

    await vi.advanceTimersByTimeAsync(1000);

    await expect(result).resolves.toBeNull();
    expect(resolvePath).not.toHaveBeenCalled();
  });

  it('does not treat exact thumbnail filename guesses as root blobs when the root is not a directory', async () => {
    vi.useFakeTimers();
    listDirectory.mockImplementation(() => new Promise(() => {}));

    const result = __test__.resolveCidWithinRoot(ROOT, 'thumbnail.jpg', {
      allowSingleSegmentRootFallback: true,
      expectedMimeType: 'image/jpeg',
    });

    await vi.advanceTimersByTimeAsync(1100);

    await expect(result).resolves.toBeNull();
    expect(resolvePath).not.toHaveBeenCalled();
  });

  it('treats an exact immutable thumbnail file path as a direct image blob when the root cid is already the file', async () => {
    vi.useFakeTimers();
    listDirectory.mockImplementation(() => new Promise(() => {}));
    readFileRange.mockResolvedValue(Uint8Array.from([
      0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46,
    ]));

    const result = __test__.resolveCidWithinRoot(ROOT_THUMB, 'thumbnail.jpg', {
      allowSingleSegmentRootFallback: true,
      expectedMimeType: 'image/jpeg',
    });

    await vi.advanceTimersByTimeAsync(1100);

    await expect(result).resolves.toBe(ROOT_THUMB);
    expect(resolvePath).not.toHaveBeenCalled();
  });

  it('treats an exact immutable thumbnail file path as a direct image blob when the file cid is keyed', async () => {
    vi.useFakeTimers();
    listDirectory.mockImplementation(() => new Promise(() => {}));
    readFileRange.mockResolvedValue(Uint8Array.from([
      0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46,
    ]));
    getBlob.mockResolvedValue(null);

    const result = __test__.resolveCidWithinRoot(KEYED_THUMB, 'thumbnail.jpg', {
      allowSingleSegmentRootFallback: true,
      expectedMimeType: 'image/jpeg',
    });

    await vi.advanceTimersByTimeAsync(1100);

    await expect(result).resolves.toBe(KEYED_THUMB);
    expect(readFileRange).toHaveBeenCalledWith(KEYED_THUMB, 0, 64);
    expect(resolvePath).not.toHaveBeenCalled();
  });

  it('resolves a video alias to a direct playable root blob when the root is already the media file', async () => {
    vi.useFakeTimers();
    listDirectory.mockImplementation(() => new Promise(() => {}));
    readFileRange.mockResolvedValue(Uint8Array.from([
      0x00, 0x00, 0x00, 0x18, 0x66, 0x74, 0x79, 0x70, 0x69, 0x73, 0x6f, 0x6d,
    ]));

    const result = __test__.resolveCidWithinRoot(ROOT, 'video', {
      allowSingleSegmentRootFallback: true,
      expectedMimeType: 'video/mp4',
    });

    await vi.advanceTimersByTimeAsync(1100);

    await expect(result).resolves.toBe(ROOT);
    await expect(__test__.normalizeAliasPath(ROOT, 'video')).resolves.toBe('video.mp4');
    expect(resolvePath).not.toHaveBeenCalled();
  });

  it('waits for a local directory listing long enough to resolve an immutable thumbnail alias', async () => {
    vi.useFakeTimers();
    listDirectory.mockImplementation(
      () =>
        new Promise((resolve) => {
          setTimeout(() => resolve([{ name: 'thumbnail.jpg', cid: ROOT_THUMB, size: 123 }]), 500);
        })
    );

    const result = __test__.resolveCidWithinRoot(ROOT, 'thumbnail', {
      allowSingleSegmentRootFallback: true,
    });

    await vi.advanceTimersByTimeAsync(500);

    await expect(result).resolves.toBe(ROOT_THUMB);
    expect(resolvePath).not.toHaveBeenCalled();
  });

  it('coalesces concurrent immutable thumbnail lookups for the same root path', async () => {
    let releaseList: ((entries: Array<{ name: string; cid: CID; size: number }>) => void) | null = null;
    listDirectory.mockImplementation(
      () =>
        new Promise((resolve) => {
          releaseList = resolve;
        })
    );

    const first = __test__.resolveCidWithinRoot(ROOT, 'thumbnail', {
      allowSingleSegmentRootFallback: true,
    });
    const second = __test__.resolveCidWithinRoot(ROOT, 'thumbnail', {
      allowSingleSegmentRootFallback: true,
    });

    await Promise.resolve();

    expect(listDirectory).toHaveBeenCalledTimes(1);

    releaseList?.([{ name: 'thumbnail.jpg', cid: ROOT_THUMB, size: 123 }]);

    await expect(first).resolves.toBe(ROOT_THUMB);
    await expect(second).resolves.toBe(ROOT_THUMB);
  });

  it('resolves a thumbnail alias from a playable file metadata nhash', async () => {
    listDirectory.mockResolvedValue([
      {
        name: 'video.mp4',
        cid: CHILD_DIR,
        size: 987,
        meta: {
          thumbnail: nhashEncode(ROOT_THUMB),
        },
      },
    ]);

    await expect(
      __test__.resolveCidWithinRoot(ROOT, 'thumbnail', { allowSingleSegmentRootFallback: true })
    ).resolves.toEqual(ROOT_THUMB);
  });

  it('resolves a thumbnail alias from metadata.json thumbnail references', async () => {
    listDirectory.mockResolvedValue([
      { name: 'metadata.json', cid: CHILD_DIR, size: 20 },
      { name: 'video.mp4', cid: KEYED_THUMB, size: 987 },
    ]);
    readFile.mockResolvedValue(new TextEncoder().encode(JSON.stringify({
      thumbnail: nhashEncode(ROOT_THUMB),
    })));

    await expect(
      __test__.resolveCidWithinRoot(ROOT, 'thumbnail', { allowSingleSegmentRootFallback: true })
    ).resolves.toEqual(ROOT_THUMB);
    expect(readFile).toHaveBeenCalledWith(CHILD_DIR);
  });

  it('resolves a thumbnail alias from a non-thumbnail image file in the directory', async () => {
    listDirectory.mockResolvedValue([
      { name: 'poster.webp', cid: ROOT_THUMB, size: 123 },
      { name: 'video.mp4', cid: KEYED_THUMB, size: 987 },
    ]);

    await expect(
      __test__.resolveCidWithinRoot(ROOT, 'thumbnail', { allowSingleSegmentRootFallback: true })
    ).resolves.toBe(ROOT_THUMB);
  });

  it('clears immutable lookup caches when initialized with a new tree', async () => {
    listDirectory.mockResolvedValue([{ name: 'thumbnail.jpg', cid: ROOT_THUMB, size: 1 }]);

    await expect(
      __test__.resolveCidWithinRoot(ROOT, 'thumbnail', { allowSingleSegmentRootFallback: true })
    ).resolves.toBe(ROOT_THUMB);
    expect(listDirectory).toHaveBeenCalledTimes(1);

    const nextListDirectory = vi.fn().mockResolvedValue([
      { name: 'thumbnail.jpg', cid: CHILD_THUMB, size: 2 },
    ]);
    initMediaHandler({
      resolvePath: vi.fn(),
      listDirectory: nextListDirectory,
    } as unknown as HashTree);

    await expect(
      __test__.resolveCidWithinRoot(ROOT, 'thumbnail', { allowSingleSegmentRootFallback: true })
    ).resolves.toBe(CHILD_THUMB);
    expect(nextListDirectory).toHaveBeenCalledTimes(1);
  });

  it('streams the actual file size when directory listings report zero-byte entry sizes', async () => {
    const rootNhash = nhashEncode(ROOT);
    const html = new TextEncoder().encode('<!doctype html><html><body>ok</body></html>');
    const postMessage = vi.fn();
    const port = {
      onmessage: null,
      postMessage,
      start: vi.fn(),
    } as unknown as MessagePort;

    listDirectory.mockImplementation(async (cid: CID) => {
      if (sameHash(cid.hash, ROOT.hash)) {
        return [{ name: 'index.html', cid: ROOT_THUMB, size: 0 }];
      }
      return null;
    });
    getBlob.mockImplementation(async (hash: Uint8Array) => {
      if (sameHash(hash, ROOT_THUMB.hash)) {
        return html;
      }
      return null;
    });
    getTreeNode.mockResolvedValue(null);
    readFileRange.mockImplementation(async (cid: CID, start: number, end?: number) => {
      if (cid !== ROOT_THUMB) return null;
      return html.slice(start, end ?? html.length);
    });

    registerMediaPort(port);

    await port.onmessage?.({
      data: {
        type: 'hashtree-file',
        requestId: 'req_1',
        nhash: rootNhash,
        path: 'index.html',
        start: 0,
        mimeType: 'text/html',
      },
    } as MessageEvent);

    expect(postMessage).toHaveBeenCalledWith(expect.objectContaining({
      type: 'headers',
      requestId: 'req_1',
      status: 200,
      headers: expect.objectContaining({
        'Content-Length': String(html.length),
        'Content-Type': 'text/html',
      }),
    }));
    expect(postMessage).toHaveBeenCalledWith(expect.objectContaining({
      type: 'chunk',
      requestId: 'req_1',
      data: html,
    }), [html.buffer]);
    expect(postMessage).toHaveBeenCalledWith(expect.objectContaining({
      type: 'done',
      requestId: 'req_1',
    }));
  });

  it('falls back to reading the full file when immutable file size metadata is unavailable', async () => {
    const rootNhash = nhashEncode(ROOT);
    const html = new TextEncoder().encode('<!doctype html><html><body>remote ok</body></html>');
    const postMessage = vi.fn();
    const port = {
      onmessage: null,
      postMessage,
      start: vi.fn(),
    } as unknown as MessagePort;

    listDirectory.mockImplementation(async (cid: CID) => {
      if (sameHash(cid.hash, ROOT.hash)) {
        return [{ name: 'index.html', cid: ROOT_THUMB, size: 0 }];
      }
      return null;
    });
    getTreeNode.mockResolvedValue(null);
    getBlob.mockResolvedValue(null);
    readFile.mockResolvedValue(html);
    readFileRange.mockImplementation(async () => null);

    registerMediaPort(port);

    await port.onmessage?.({
      data: {
        type: 'hashtree-file',
        requestId: 'req_2',
        nhash: rootNhash,
        path: 'index.html',
        start: 0,
        mimeType: 'text/html',
      },
    } as MessageEvent);

    expect(readFile).toHaveBeenCalledWith(ROOT_THUMB);
    expect(postMessage).toHaveBeenCalledWith(expect.objectContaining({
      type: 'headers',
      requestId: 'req_2',
      status: 200,
      headers: expect.objectContaining({
        'Content-Length': String(html.length),
        'Content-Type': 'text/html',
      }),
    }));
    expect(postMessage).toHaveBeenCalledWith(expect.objectContaining({
      type: 'chunk',
      requestId: 'req_2',
      data: html,
    }), [html.buffer]);
    expect(postMessage).toHaveBeenCalledWith(expect.objectContaining({
      type: 'done',
      requestId: 'req_2',
    }));
  });

  it('streams unknown-size release downloads without buffering the full file first', async () => {
    const rootNhash = nhashEncode(ROOT);
    const firstChunk = new Uint8Array([1, 2, 3, 4]);
    const postMessage = vi.fn();
    const port = {
      onmessage: null,
      postMessage,
      start: vi.fn(),
    } as unknown as MessagePort;

    listDirectory.mockImplementation(async (cid: CID) => {
      if (sameHash(cid.hash, ROOT.hash)) {
        return [{ name: 'iris-drive.dmg', cid: ROOT_THUMB, size: 0 }];
      }
      return null;
    });
    getTreeNode.mockResolvedValue(null);
    getBlob.mockResolvedValue(null);
    readFile.mockResolvedValue(firstChunk);

    let resolveStream: ((data: Uint8Array | null) => void) | null = null;
    readFileStream.mockImplementation(async function* () {
      const chunk = await new Promise<Uint8Array | null>((resolve) => {
        resolveStream = resolve;
      });
      if (chunk) {
        yield chunk;
      }
    });

    registerMediaPort(port);

    const request = port.onmessage?.({
      data: {
        type: 'hashtree-file',
        requestId: 'req_download',
        nhash: rootNhash,
        path: 'iris-drive.dmg',
        start: 0,
        mimeType: 'application/octet-stream',
        download: true,
      },
    } as MessageEvent);

    await vi.waitFor(() => {
      expect(postMessage).toHaveBeenCalledWith(expect.objectContaining({
        type: 'headers',
        requestId: 'req_download',
        status: 200,
        headers: expect.objectContaining({
          'Content-Disposition': 'attachment; filename="iris-drive.dmg"',
          'Content-Type': 'application/octet-stream',
        }),
        totalSize: 0,
      }));
    });
    expect(readFile).not.toHaveBeenCalled();
    expect(postMessage).not.toHaveBeenCalledWith(expect.objectContaining({
      type: 'chunk',
      requestId: 'req_download',
    }), expect.anything());

    resolveStream?.(firstChunk);
    await request;

    expect(postMessage).toHaveBeenCalledWith(expect.objectContaining({
      type: 'chunk',
      requestId: 'req_download',
      data: firstChunk,
    }), [firstChunk.buffer]);
    expect(postMessage).toHaveBeenCalledWith(expect.objectContaining({
      type: 'done',
      requestId: 'req_download',
    }));
  });

  it('resolves immutable snapshot requests through the signed root-event snapshot', async () => {
    const store = new MemoryStore();
    const realTree = new HashTree({ store });
    const html = new TextEncoder().encode('<!doctype html><html><body>snapshot ok</body></html>');
    const { cid: fileCid, size } = await realTree.putFile(html, { unencrypted: true });
    const { cid: rootCid } = await realTree.putDirectory([
      { name: 'index.html', cid: fileCid, size },
    ], { unencrypted: true });
    const snapshot = await storeTreeEventSnapshot(store, nip19, {
      id: '8'.repeat(64),
      pubkey: '9'.repeat(64),
      kind: 30078,
      content: '',
      tags: [
        ['d', 'sites/snapshot'],
        ['l', 'hashtree'],
        ['hash', toHex(rootCid.hash)],
      ],
      created_at: 1_700_000_100,
      sig: 'a'.repeat(128),
    });

    expect(snapshot?.snapshotNhash).toMatch(/^nhash1/);

    const postMessage = vi.fn();
    const port = {
      onmessage: null,
      postMessage,
      start: vi.fn(),
    } as unknown as MessagePort;

    initMediaHandler(realTree);
    registerMediaPort(port);

    await port.onmessage?.({
      data: {
        type: 'hashtree-file',
        requestId: 'req_snapshot',
        nhash: snapshot!.snapshotNhash,
        snapshot: true,
        path: 'index.html',
        start: 0,
        mimeType: 'text/html',
      },
    } as MessageEvent);

    expect(postMessage).toHaveBeenCalledWith(expect.objectContaining({
      type: 'headers',
      requestId: 'req_snapshot',
      status: 200,
      headers: expect.objectContaining({
        'Content-Length': String(html.length),
        'Content-Type': 'text/html',
      }),
    }));
    expect(postMessage).toHaveBeenCalledWith(expect.objectContaining({
      type: 'chunk',
      requestId: 'req_snapshot',
      data: html,
    }), [html.buffer]);
    expect(postMessage).toHaveBeenCalledWith(expect.objectContaining({
      type: 'done',
      requestId: 'req_snapshot',
    }));
  });
});
