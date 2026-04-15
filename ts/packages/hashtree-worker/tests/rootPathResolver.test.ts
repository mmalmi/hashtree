import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { CID } from '@hashtree/core';
import { DEFAULT_ROOT_PATH_RESOLVE_TIMEOUT_MS, parseRootPath, resolveRootPath } from '../src/relay/rootPathResolver';

const { resolveTreeRootNow } = vi.hoisted(() => ({
  resolveTreeRootNow: vi.fn(),
}));

vi.mock('../src/relay/treeRootSubscription', () => ({
  resolveTreeRootNow,
}));

const ROOT: CID = { hash: Uint8Array.from({ length: 32 }, (_, i) => i + 1) };
const CHILD: CID = { hash: Uint8Array.from({ length: 32 }, (_, i) => i + 33) };
const NPUB = 'npub1g53mukxnjkcmr94fhryzkqutdz2ukq4ks0gvy5af25rgmwsl4ngq43drvk';

describe('rootPathResolver', () => {
  beforeEach(() => {
    vi.resetModules();
    resolveTreeRootNow.mockReset();
  });

  it('resolves a subpath from the fetched tree root', async () => {
    resolveTreeRootNow.mockResolvedValue(ROOT);
    const resolvePath = vi.fn().mockResolvedValue({ cid: CHILD });

    await expect(resolveRootPath({ resolvePath }, NPUB, 'videos/Mine Bombers in-game music'))
      .resolves.toEqual(CHILD);

    expect(resolveTreeRootNow).toHaveBeenCalledTimes(1);
    expect(resolveTreeRootNow).toHaveBeenCalledWith(NPUB, 'videos', DEFAULT_ROOT_PATH_RESOLVE_TIMEOUT_MS);
    expect(resolvePath).toHaveBeenCalledWith(ROOT, ['Mine Bombers in-game music']);
  });

  it('returns the tree root when the path points at the tree itself', async () => {
    const resolvePath = vi.fn();
    resolveTreeRootNow.mockResolvedValue(ROOT);

    await expect(resolveRootPath({ resolvePath }, NPUB, 'videos')).resolves.toEqual(ROOT);

    expect(resolvePath).not.toHaveBeenCalled();
  });

  it('keeps public as the default tree name when no path is provided', async () => {
    resolveTreeRootNow.mockResolvedValue(null);

    await expect(resolveRootPath({ resolvePath: vi.fn() }, NPUB)).resolves.toBeNull();

    expect(resolveTreeRootNow).toHaveBeenCalledWith(NPUB, 'public', DEFAULT_ROOT_PATH_RESOLVE_TIMEOUT_MS);
  });

  it('parses root paths into tree and subpath segments', () => {
    expect(parseRootPath('videos/Music/video_123')).toEqual({
      treeName: 'videos',
      subPath: ['Music', 'video_123'],
    });
    expect(parseRootPath('videos/Music%20video')).toEqual({
      treeName: 'videos',
      subPath: ['Music video'],
    });
    expect(parseRootPath()).toEqual({
      treeName: 'public',
      subPath: [],
    });
  });

  it('resolves nested paths relative to the fetched root', async () => {
    resolveTreeRootNow.mockResolvedValue(ROOT);
    const resolvePath = vi.fn().mockResolvedValue({ cid: CHILD });

    await expect(resolveRootPath({ resolvePath }, NPUB, 'repo/src%20dir/index.ts')).resolves.toEqual(CHILD);

    expect(resolveTreeRootNow).toHaveBeenCalledWith(NPUB, 'repo', DEFAULT_ROOT_PATH_RESOLVE_TIMEOUT_MS);
    expect(resolvePath).toHaveBeenCalledWith(ROOT, ['src dir', 'index.ts']);
  });

  it('returns null when the tree root cannot be resolved', async () => {
    resolveTreeRootNow.mockResolvedValue(null);

    await expect(resolveRootPath({ resolvePath: vi.fn() }, NPUB, 'videos/Mine Bombers in-game music')).resolves.toBeNull();

    expect(resolveTreeRootNow).toHaveBeenCalledWith(NPUB, 'videos', DEFAULT_ROOT_PATH_RESOLVE_TIMEOUT_MS);
  });
});
