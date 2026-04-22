import { describe, it, expect, beforeEach } from 'vitest';
import { MemoryStore, type CID } from '@hashtree/core';
import { SearchIndex } from '../src/search.js';

describe('SearchIndex', () => {
  let store: MemoryStore;
  let index: SearchIndex;

  beforeEach(() => {
    store = new MemoryStore();
    index = new SearchIndex(store, { order: 4 });
  });

  it('keeps whole tokens and splits camelCase variants', () => {
    expect(index.parseKeywords('SirLibre')).toEqual(['sirlibre', 'sir', 'libre']);
    expect(index.parseKeywords('XMLHttpRequest42')).toEqual([
      'xmlhttprequest42',
      'xml',
      'http',
      'request',
    ]);
  });

  it('ranks exact keyword matches ahead of longer prefix matches', async () => {
    let root = null;
    root = await index.index(root, 'p:', ['petrix'], 'pubkey-petrix', '{"name":"petrix"}');
    root = await index.index(root, 'p:', ['petri'], 'pubkey-petri', '{"name":"petri"}');

    const results = await index.search(root, 'p:', 'petri', { limit: 10 });

    expect(results.map((result) => result.id)).toEqual(['pubkey-petri', 'pubkey-petrix']);
  });

  it('searches caller-supplied terms without re-parsing them', async () => {
    const singleCharIndex = new SearchIndex(store, {
      order: 4,
      minKeywordLength: 1,
    });
    const cidA: CID = { hash: new Uint8Array(32).fill(1) };
    const cidB: CID = { hash: new Uint8Array(32).fill(2) };
    let root = null;
    root = await singleCharIndex.indexLink(root, 's:', ['a', 'anthem'], 'song-a', cidA);
    root = await singleCharIndex.indexLink(root, 's:', ['anthem'], 'song-b', cidB);

    expect(await singleCharIndex.searchLinks(root, 's:', 'a', {
      limit: 10,
      fullMatch: true,
    })).toEqual([]);
    expect(await singleCharIndex.searchLinkTerms(root, 's:', ['A'], {
      limit: 10,
      scanLimit: 10,
      fullMatch: true,
    })).toEqual([
      { id: 'song-a', cid: cidA, score: 1, exactMatches: 1, prefixDistance: 0 },
    ]);
  });
});
