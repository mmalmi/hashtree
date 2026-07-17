import { HashTree, MemoryStore, type CID } from '@hashtree/core';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import {
  RankedSearchIndex,
  type RankedSearchBuildOptions,
  type RankedSearchCorpusStatistics,
  type RankedSearchDocument,
  type RankedSearchScoringContext,
  type RankedTermStats,
} from '../src/index.js';

const buildOptions: RankedSearchBuildOptions = {
  fields: {
    title: { boost: 4, lengthNormalization: 0.3 },
    body: { boost: 1, lengthNormalization: 0.75 },
  },
  maxTokensPerField: 128,
};

describe('RankedSearchIndex', () => {
  let store: MemoryStore;
  let index: RankedSearchIndex;

  beforeEach(() => {
    store = new MemoryStore();
    index = new RankedSearchIndex(store, { order: 4 });
  });

  it('ranks weighted fields with BM25F length normalization', async () => {
    const root = await index.buildSegment([
      document('title-hit', 'Nostr indexing', 'A compact implementation', 'title'),
      document(
        'body-hit',
        'Indexing notes',
        `Nostr ${'background material '.repeat(30)}`,
        'body',
      ),
      document(
        'frequent-body',
        'Protocol notes',
        `nostr nostr nostr ${'reference '.repeat(20)}`,
        'frequent',
      ),
    ], buildOptions);

    const results = await index.search(root, 'nostr');

    expect(results.map((result) => result.id)).toEqual([
      'title-hit',
      'frequent-body',
      'body-hit',
    ]);
    expect(results[0].score).toBeGreaterThan(results[1].score);
    expect(results[0].value).toBe('title');
  });

  it('normalizes Unicode compatibility forms and combining marks', async () => {
    const root = await index.buildSegment([
      document('accent', 'Cafe\u0301 society', '', 'accent'),
      document('width', 'Ｎｏｓｔｒ protocol', '', 'width'),
    ], buildOptions);

    expect((await index.search(root, 'CAFÉ')).map((result) => result.id)).toEqual(['accent']);
    expect((await index.search(root, 'nostr')).map((result) => result.id)).toEqual(['width']);
  });

  it('supports ranked multi-term OR and strict AND queries', async () => {
    const root = await index.buildSegment([
      document('both', 'decentralized nostr', '', 'both'),
      document('nostr-only', 'nostr relay', '', 'nostr'),
      document('decentralized-only', 'decentralized web', '', 'decentralized'),
    ], buildOptions);

    const orResults = await index.search(root, 'decentralized nostr');
    const andResults = await index.search(root, 'decentralized nostr', { operator: 'and' });

    expect(orResults[0].id).toBe('both');
    expect(andResults.map((result) => result.id)).toEqual(['both']);
    expect(andResults[0].matchedTerms).toEqual(['decentralized', 'nostr']);
  });

  it('uses bounded positional postings for phrases and exact hashtags', async () => {
    const root = await index.buildSegment([
      document('phrase', 'decentralized social graph', '#Nostr builders', 'phrase'),
      document('separated', 'decentralized fast social graph', 'nostr builders', 'separated'),
    ], buildOptions);

    expect((await index.search(root, '"decentralized social"')).map((result) => result.id))
      .toEqual(['phrase']);
    expect((await index.search(root, '#nostr')).map((result) => result.id))
      .toEqual(['phrase']);
  });

  it('computes document frequency for only the selected fields', async () => {
    const documents = [
      document('a', 'alpha', '', 'a'),
      document('b', 'beta', '', 'b'),
      ...Array.from({ length: 12 }, (_, number) =>
        document(`noise-${number}`, 'noise', 'alpha', `noise-${number}`)),
    ];
    const root = await index.buildSegment(documents, buildOptions);

    const results = await index.search(root, 'alpha beta', { fields: ['title'] });

    expect(results.map((result) => result.id)).toEqual(['a', 'b']);
    expect(results[0].score).toBeCloseTo(results[1].score, 12);
  });

  it('matches phrases after many repeated occurrences within the field token limit', async () => {
    const root = await index.buildSegment([
      document('late', `${'alpha x '.repeat(70)}alpha beta`, '', 'late'),
    ], { ...buildOptions, maxTokensPerField: 256 });

    expect((await index.search(root, '"alpha beta"')).map((result) => result.id))
      .toEqual(['late']);
  });

  it('supports reserved JavaScript property names as fields', async () => {
    const fields = Object.fromEntries([
      ['constructor', { boost: 2 }],
      ['__proto__', { boost: 1 }],
    ]);
    const documentFields = Object.fromEntries([
      ['constructor', 'reserved constructor'],
      ['__proto__', 'reserved prototype'],
    ]);
    const root = await index.buildSegment([
      { id: 'reserved', fields: documentFields, value: 'reserved' },
    ], { fields });

    expect((await index.search(root, 'constructor', { fields: ['constructor'] }))[0].id)
      .toBe('reserved');
    expect((await index.search(root, 'prototype', { fields: ['__proto__'] }))[0].id)
      .toBe('reserved');
  });

  it('seeds AND queries from the rarest postings list', async () => {
    const documents = Array.from({ length: 96 }, (_, number) =>
      document(
        `doc-${number.toString().padStart(3, '0')}`,
        number < 2 ? 'common rare' : 'common',
        '',
        String(number),
      ));
    const root = await index.buildSegment(documents, buildOptions);
    const get = vi.spyOn(store, 'get');

    const andResults = await index.search(root, 'common rare', {
      operator: 'and',
      limit: 2,
    });
    const andReads = get.mock.calls.length;
    get.mockClear();
    const orResults = await index.search(root, 'common rare', { limit: 2 });
    const orReads = get.mock.calls.length;

    expect(andResults.map((result) => result.id)).toEqual(['doc-000', 'doc-001']);
    expect(orResults.map((result) => result.id)).toEqual(['doc-000', 'doc-001']);
    expect(andReads).toBeLessThan(orReads / 2);
  });

  it('bounds common-term top-k reads without scanning the posting list', async () => {
    const productionIndex = new RankedSearchIndex(store, { order: 64 });
    const documents = [
      ...Array.from({ length: 32 }, (_, number) =>
        document(
          `fast-${number.toString().padStart(4, '0')}`,
          'common',
          '',
          `fast-${number}`,
        )),
      ...Array.from({ length: 2016 }, (_, number) =>
        document(
          `slow-${number.toString().padStart(4, '0')}`,
          `common ${'padding '.repeat(48)}`,
          '',
          `slow-${number}`,
        )),
    ];
    const root = await productionIndex.buildSegment(documents, buildOptions);
    const get = vi.spyOn(store, 'get');

    const results = await productionIndex.search(root, 'common', { limit: 10 });
    const acceleratedReads = get.mock.calls.length;
    const legacyRoot = await withoutTopK(store, root);
    get.mockClear();
    const legacyResults = await productionIndex.search(legacyRoot, 'common', { limit: 10 });
    const legacyReads = get.mock.calls.length;

    expect(results.map((result) => result.id)).toEqual(
      documents.slice(0, 10).map((item) => item.id),
    );
    expect(results).toEqual(legacyResults);
    expect(acceleratedReads).toBeLessThan(documents.length / 6);
    expect(acceleratedReads).toBeLessThan(legacyReads / 8);
  });

  it('omits sparse vocabulary blocks and keeps mixed common-rare queries bounded', async () => {
    const productionIndex = new RankedSearchIndex(store, { order: 64 });
    const documents = Array.from({ length: 1024 }, (_, number) => document(
      `vocab-${number.toString().padStart(4, '0')}`,
      [
        ...Array.from({ length: number < 32 ? 33 - number : 1 }, () => 'common'),
        `topicx${number % 16}`,
        `rarex${number.toString().padStart(4, '0')}`,
        `authorx${number.toString().padStart(4, '0')}`,
      ].join(' '),
      '',
      `vocab-${number}`,
    ));
    const root = await productionIndex.buildSegment(documents, buildOptions);
    const legacyRoot = await withoutTopK(store, root);
    const acceleratedStorage = await reachableStats(store, root);
    const legacyStorage = await reachableStats(store, legacyRoot);
    const topKManifest = await readTopKManifest(store, root);
    const get = vi.spyOn(store, 'get');

    const results = await productionIndex.search(root, 'common rarex0007', { limit: 10 });
    const acceleratedReads = get.mock.calls.length;
    get.mockClear();
    const legacyResults = await productionIndex.search(
      legacyRoot,
      'common rarex0007',
      { limit: 10 },
    );
    const legacyReads = get.mock.calls.length;

    expect(results).toEqual(legacyResults);
    expect(results[0].id).toBe('vocab-0007');
    expect(acceleratedReads).toBeLessThan(legacyReads / 8);
    expect(topKManifest).toMatchObject({
      format: 'hashtree/ranked-top-k@1',
      minimumDocumentFrequency: 128,
      termCount: 1,
      blockCount: 32,
      postingCount: 1024,
    });
    expect(acceleratedStorage.blocks).toBeGreaterThan(legacyStorage.blocks);
    expect(acceleratedStorage.bytes - legacyStorage.bytes)
      .toBeLessThan(legacyStorage.bytes / 10);
  });

  it('keeps exact id ordering for equal-score common terms', async () => {
    const productionIndex = new RankedSearchIndex(store, { order: 64 });
    const documents = Array.from({ length: 2048 }, (_, number) =>
      document(
        `same-${number.toString().padStart(4, '0')}`,
        'common',
        '',
        `same-${number}`,
      ));
    const root = await productionIndex.buildSegment(documents, buildOptions);
    const results = await productionIndex.search(root, 'common', { limit: 10 });

    expect(results.map((result) => result.id)).toEqual(
      documents.slice(0, 10).map((item) => item.id),
    );
  });

  it('keeps accelerated scores and ordering exact with legacy segment reads', async () => {
    const documents = Array.from({ length: 256 }, (_, number) => document(
      `doc-${number.toString().padStart(4, '0')}`,
      [
        'common',
        ...(number % 3 === 0 ? ['alpha'] : []),
        ...(number % 5 === 0 ? ['beta'] : []),
        ...Array.from({ length: number % 11 }, () => 'padding'),
      ].join(' '),
      number % 7 === 0 ? 'common alpha body' : `background ${'tail '.repeat(number % 13)}`,
      `value-${number}`,
    ));
    const fullRoot = await index.buildSegment(documents, buildOptions);
    const root = await index.buildSegment(documents.slice(0, 128), buildOptions);
    const legacyRoot = await withoutTopK(store, root);
    const scoringContext = await scoringContextFor(
      index,
      fullRoot,
      ['common', 'alpha', 'beta'],
    );

    for (const [query, options] of [
      ['common', { limit: 10, scoringContext }],
      ['common alpha beta', { limit: 17, scoringContext }],
      ['"common alpha"', { limit: 25, scoringContext }],
      ['common alpha', { limit: 12, fields: ['body'], scoringContext }],
    ] as const) {
      expect(await index.search(root, query, options))
        .toEqual(await index.search(legacyRoot, query, options));
    }
    expect((await index.readManifest(root)).format).toBe('hashtree/ranked-search-segment@1');
  });

  it('matches legacy search across deterministic global multi-field query fuzz', async () => {
    const fuzzIndex = new RankedSearchIndex(store, { order: 64 });
    const options: RankedSearchBuildOptions = {
      fields: {
        title: { boost: 3.7, lengthNormalization: 1 },
        body: { boost: 0.9, lengthNormalization: 0.37 },
      },
      k1: 1.63,
      maxTokensPerField: 128,
    };
    const documents = generatedDocuments(512, 0x5eed1234);
    const fullRoot = await fuzzIndex.buildSegment(documents, options);
    const root = await fuzzIndex.buildSegment(documents.slice(0, 384), options);
    const legacyRoot = await withoutTopK(store, root);
    const scoringContext = await scoringContextFor(fuzzIndex, fullRoot, [
      'common',
      'alpha',
      'beta',
      'gamma',
      'delta',
      '#nostr',
      'rarex0007',
    ]);
    const queries = [
      'common',
      'common alpha',
      'beta gamma delta',
      '"common alpha"',
      '#nostr common',
      'common rarex0007',
      'alpha delta',
      '"beta gamma" common',
    ] as const;

    for (let iteration = 0; iteration < 48; iteration += 1) {
      const query = queries[iteration % queries.length];
      const fields = iteration % 3 === 0
        ? ['title'] as const
        : iteration % 3 === 1
          ? ['body'] as const
          : undefined;
      const searchOptions = {
        limit: 1 + (iteration * 7) % 37,
        ...(fields ? { fields } : {}),
        ...(iteration % 2 === 0 ? { scoringContext } : {}),
      };
      expect(await fuzzIndex.search(root, query, searchOptions))
        .toEqual(await fuzzIndex.search(legacyRoot, query, searchOptions));
    }
  }, 20_000);

  it('builds the same content-addressed root regardless of input ordering', async () => {
    const documents = [
      document('a', 'Nostr search', 'first body', 'a'),
      document('b', 'Hashtree index', 'second body', 'b'),
    ];
    const first = await index.buildSegment(documents, buildOptions);

    const otherStore = new MemoryStore();
    const otherIndex = new RankedSearchIndex(otherStore, { order: 4 });
    const reversedOptions: RankedSearchBuildOptions = {
      ...buildOptions,
      fields: {
        body: buildOptions.fields.body,
        title: buildOptions.fields.title,
      },
    };
    const second = await otherIndex.buildSegment([...documents].reverse(), reversedOptions);

    expect(cidBytes(first)).toEqual(cidBytes(second));
    const manifest = await index.readManifest(first);
    expect(manifest).toEqual(await otherIndex.readManifest(second));
    expect({
      documents: manifest.documentCount,
      terms: manifest.termCount,
      postings: manifest.postingCount,
      values: manifest.storedValueCount,
    }).toEqual({ documents: 2, terms: 7, postings: 8, values: 2 });
    expect(manifest.fields.map((field) => ({
      name: field.name,
      totalLength: field.totalLength,
      populatedDocumentCount: field.populatedDocumentCount,
    }))).toEqual([
      { name: 'body', totalLength: 4, populatedDocumentCount: 2 },
      { name: 'title', totalLength: 4, populatedDocumentCount: 2 },
    ]);
  });

  it('rejects duplicate document ids instead of making roots order-dependent', async () => {
    const duplicate: RankedSearchDocument[] = [
      document('same', 'first', '', 'one'),
      document('same', 'second', '', 'two'),
    ];

    await expect(index.buildSegment(duplicate, buildOptions))
      .rejects.toThrow('Duplicate ranked search document id: same');
  });

  it('streams and reads exact term statistics', async () => {
    const root = await index.buildSegment([
      document('both', 'needle', 'needle body', 'both'),
      document('title', 'needle', 'background', 'title'),
      document('other', 'background', 'other', 'other'),
    ], buildOptions);

    const streamed = new Map<string, RankedTermStats>();
    for await (const [term, statistics] of index.streamTermStatistics(root)) {
      streamed.set(term, statistics);
    }
    const selected = await index.readTermStatistics(root, ['needle', 'missing']);

    expect([...streamed.keys()]).toEqual([...streamed.keys()].sort());
    expect(selected.get('needle')).toEqual({
      documentFrequency: 2,
      fieldSets: [
        { fields: ['body', 'title'], documentFrequency: 1 },
        { fields: ['title'], documentFrequency: 1 },
      ],
    });
    expect(selected.has('missing')).toBe(false);
  });

  it('keeps global BM25F scores identical across segment splits', async () => {
    const documents = [
      document('newer', 'needle', '', 'newer'),
      document('older', 'needle', '', 'older'),
      ...Array.from({ length: 50 }, (_, number) =>
        document(`noise-${number.toString().padStart(2, '0')}`, 'background', '', `noise-${number}`)),
    ];
    const fullRoot = await index.buildSegment(documents, buildOptions);
    const newerRoot = await index.buildSegment(documents.slice(0, 1), buildOptions);
    const olderRoot = await index.buildSegment(documents.slice(1), buildOptions);
    const scoringContext = await scoringContextFor(index, fullRoot, ['needle']);

    const unsharded = await index.search(fullRoot, 'needle', { scoringContext });
    const sharded = [
      ...await index.search(newerRoot, 'needle', { scoringContext }),
      ...await index.search(olderRoot, 'needle', { scoringContext }),
    ].sort((left, right) => right.score - left.score || left.id.localeCompare(right.id));

    expect(sharded.map((result) => result.id)).toEqual(unsharded.map((result) => result.id));
    expect(sharded.map((result) => result.score)).toEqual(unsharded.map((result) => result.score));
  });

  it('uses global field-set document frequencies for selected fields', async () => {
    const documents = [
      document('both', 'needle', 'needle', 'both'),
      document('title', 'needle', 'background', 'title'),
      document('body', 'background', 'needle', 'body'),
    ];
    const fullRoot = await index.buildSegment(documents, buildOptions);
    const splitRoot = await index.buildSegment(documents.slice(0, 1), buildOptions);
    const scoringContext = await scoringContextFor(index, fullRoot, ['needle']);

    const fullBody = await index.search(fullRoot, 'needle', {
      fields: ['body'],
      scoringContext,
    });
    const splitBody = await index.search(splitRoot, 'needle', {
      fields: ['body'],
      scoringContext,
    });
    const fullTitle = await index.search(fullRoot, 'needle', {
      fields: ['title'],
      scoringContext,
    });
    const splitTitle = await index.search(splitRoot, 'needle', {
      fields: ['title'],
      scoringContext,
    });

    expect(splitBody[0].score).toBe(fullBody.find((result) => result.id === 'both')?.score);
    expect(splitTitle[0].score).toBe(fullTitle.find((result) => result.id === 'both')?.score);
    expect(splitBody[0].score).not.toBe(splitTitle[0].score);
  });

  it('rejects tampered global scoring statistics', async () => {
    const fullRoot = await index.buildSegment([
      document('one', 'needle', '', 'one'),
      document('two', 'background', '', 'two'),
    ], buildOptions);
    const splitRoot = await index.buildSegment([
      document('one', 'needle', '', 'one'),
    ], buildOptions);
    const valid = await scoringContextFor(index, fullRoot, ['needle']);

    await expect(index.search(splitRoot, 'needle', {
      scoringContext: {
        ...valid,
        corpus: { ...valid.corpus, documentCount: 0 },
      },
    })).rejects.toThrow('document count');
    await expect(index.search(splitRoot, 'needle', {
      scoringContext: {
        ...valid,
        corpus: {
          ...valid.corpus,
          fields: valid.corpus.fields.map((field) => field.name === 'title'
            ? { ...field, totalLength: 0 }
            : field),
        },
      },
    })).rejects.toThrow('field totals');
    await expect(index.search(splitRoot, 'needle', {
      scoringContext: { ...valid, termStatistics: new Map() },
    })).rejects.toThrow('Missing global ranked term statistics');
    await expect(index.search(splitRoot, 'needle', {
      scoringContext: {
        ...valid,
        termStatistics: new Map([['needle', {
          documentFrequency: 0,
          fieldSets: [],
        }]]),
      },
    })).rejects.toThrow('term statistics');
  });
});

async function scoringContextFor(
  index: RankedSearchIndex,
  root: CID,
  terms: readonly string[],
): Promise<RankedSearchScoringContext> {
  const manifest = await index.readManifest(root);
  const corpus: RankedSearchCorpusStatistics = {
    documentCount: manifest.documentCount,
    k1: manifest.k1,
    fields: manifest.fields,
  };
  return {
    corpus,
    termStatistics: await index.readTermStatistics(root, terms),
  };
}

function document(
  id: string,
  title: string,
  body: string,
  value: string,
): RankedSearchDocument {
  return { id, fields: { title, body }, value };
}

function generatedDocuments(count: number, seed: number): RankedSearchDocument[] {
  let state = seed >>> 0;
  const next = (): number => {
    state = (Math.imul(state, 1664525) + 1013904223) >>> 0;
    return state;
  };
  return Array.from({ length: count }, (_, number) => {
    const title = ['common'];
    const body: string[] = [];
    if (next() % 2 === 0) title.push('alpha');
    if (next() % 3 !== 0) title.push('beta', 'gamma');
    if (next() % 4 === 0) title.push('#nostr');
    if (next() % 2 === 0) body.push('common');
    if (next() % 3 === 0) body.push('alpha', 'delta');
    if (next() % 5 !== 0) body.push('beta', 'gamma');
    title.push(...Array.from({ length: next() % 9 }, () => 'padding'));
    body.push(...Array.from({ length: next() % 13 }, () => 'tail'));
    title.push(`rarex${number.toString().padStart(4, '0')}`);
    return document(
      `fuzz-${number.toString().padStart(4, '0')}`,
      title.join(' '),
      body.join(' '),
      `fuzz-${number}`,
    );
  });
}

function cidBytes(cid: CID): { hash: number[]; key?: number[] } {
  return {
    hash: [...cid.hash],
    ...(cid.key ? { key: [...cid.key] } : {}),
  };
}

async function withoutTopK(store: MemoryStore, root: CID): Promise<CID> {
  const tree = new HashTree({ store });
  const entries = await tree.listDirectory(root);
  return (await tree.putDirectory(
    entries.filter((entry) => entry.name !== 'top-k-roots' && entry.name !== 'top-k.json'),
  )).cid;
}

async function reachableStats(
  store: MemoryStore,
  root: CID,
): Promise<{ blocks: number; bytes: number }> {
  const tree = new HashTree({ store });
  let blocks = 0;
  let bytes = 0;
  for await (const block of tree.walkBlocks(root)) {
    blocks += 1;
    bytes += block.data.length;
  }
  return { blocks, bytes };
}

async function readTopKManifest(
  store: MemoryStore,
  root: CID,
): Promise<Record<string, unknown>> {
  const tree = new HashTree({ store });
  const entry = (await tree.listDirectory(root)).find((item) => item.name === 'top-k.json');
  if (!entry) throw new Error('missing ranked top-k test manifest');
  const bytes = await tree.readFile(entry.cid);
  if (!bytes) throw new Error('unreadable ranked top-k test manifest');
  return JSON.parse(new TextDecoder().decode(bytes)) as Record<string, unknown>;
}
