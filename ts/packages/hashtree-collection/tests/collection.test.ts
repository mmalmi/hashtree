import { describe, expect, it } from 'vitest';
import type { CID } from '@hashtree/core';
import { MemoryStore } from '@hashtree/core';
import {
  CollectionSource,
  CollectionWriter,
  collectionManifestMetadataFromManifest,
  federatedSearch,
  parseCollectionManifestMetadata,
  serializeCollectionManifestMetadata,
  type CollectionDefinition,
  normalizeCollectionItem,
} from '../src/index.js';

interface Song {
  id: string;
  title: string;
  artist: string;
  tags?: string[];
}

function sequenceRandom(sequence: number[]): () => number {
  let index = 0;
  return () => sequence[index++ % sequence.length] ?? 0;
}

function cidFromSeed(seed: number): CID {
  const hash = new Uint8Array(32);
  for (let index = 0; index < hash.length; index += 1) {
    hash[index] = (seed + index) & 0xff;
  }
  return { hash };
}

const songDefinition: CollectionDefinition<Song> = {
  sourceId: 'npub1test/audio',
  schemaVersion: 1,
  getId: (song) => song.id,
  keyIndexes: [
    {
      name: 'artist',
      keys: (song) => [`artist:${song.artist.toLowerCase()}`],
    },
  ],
  searchIndexes: [
    {
      name: 'songs',
      prefix: 's:',
      text: (song) => [song.title, song.artist, ...(song.tags ?? [])],
    },
  ],
};

const byIdOnlySongDefinition: CollectionDefinition<Song> = {
  sourceId: 'npub1test/audio/by-id-only',
  schemaVersion: 1,
  getId: (song) => song.id,
};

describe('@hashtree/collection', () => {
  it('autoupdates by-id, key, and search indexes on put and delete', async () => {
    const store = new MemoryStore();
    const writer = new CollectionWriter(store, songDefinition);
    const songA: Song = { id: 'song-a', title: 'Midnight Orchard', artist: 'Ada', tags: ['dream-pop'] };
    const songB: Song = { id: 'song-b', title: 'Sun Clock', artist: 'Bea', tags: ['ambient'] };

    await writer.put(songA, cidFromSeed(1));
    await writer.put(songB, cidFromSeed(2));

    let source = new CollectionSource(store, writer.manifest());
    expect(await source.get('song-a')).toEqual(cidFromSeed(1));
    expect(await source.count()).toBe(2);
    expect((await source.queryById()).map((result) => result.key)).toEqual(['song-a', 'song-b']);
    expect((await source.search('songs', 'midnight')).map((result) => result.id)).toEqual(['song-a']);
    expect((await source.queryIndex('artist', { prefix: 'artist:ada' })).map((result) => result.key)).toEqual(['artist:ada']);
    expect(source.manifest.itemCount).toBe(2);

    await writer.delete(songA);
    source = new CollectionSource(store, writer.manifest());
    expect(await source.get('song-a')).toBeNull();
    expect(await source.count()).toBe(1);
    expect((await source.queryById()).map((result) => result.key)).toEqual(['song-b']);
    expect(await source.search('songs', 'midnight')).toEqual([]);
    expect(await source.queryIndex('artist', { prefix: 'artist:ada' })).toEqual([]);
    expect(source.manifest.itemCount).toBe(1);
  });

  it('samples by id using manifest item counts', async () => {
    const store = new MemoryStore();
    const writer = new CollectionWriter(store, songDefinition);

    await writer.put({ id: 'song-a', title: 'Midnight Orchard', artist: 'Ada' }, cidFromSeed(3));
    await writer.put({ id: 'song-b', title: 'Sun Clock', artist: 'Bea' }, cidFromSeed(4));
    await writer.put({ id: 'song-c', title: 'Silent Tide', artist: 'Cia' }, cidFromSeed(5));

    const source = new CollectionSource(store, writer.manifest());
    expect(await source.sampleById(2, sequenceRandom([0.1, 0.9, 0.3]))).toEqual([
      { key: 'song-c', cid: cidFromSeed(5) },
      { key: 'song-a', cid: cidFromSeed(3) },
    ]);
  });

  it('streams by-id and key indexes without materializing the whole result set first', async () => {
    const store = new MemoryStore();
    const writer = new CollectionWriter(store, songDefinition);

    await writer.put({ id: 'song-a', title: 'Midnight Orchard', artist: 'Ada' }, cidFromSeed(6));
    await writer.put({ id: 'song-b', title: 'Sun Clock', artist: 'Ada' }, cidFromSeed(7));
    await writer.put({ id: 'song-c', title: 'Silent Tide', artist: 'Bea' }, cidFromSeed(8));

    const source = new CollectionSource(store, writer.manifest());
    const byIdKeys: string[] = [];
    for await (const result of source.streamQueryById({ prefix: 'song-', limit: 2 })) {
      byIdKeys.push(result.key);
    }

    const artistKeys: string[] = [];
    for await (const result of source.streamQueryIndex('artist', { prefix: 'artist:ada', limit: 1 })) {
      artistKeys.push(result.key);
    }

    expect(byIdKeys).toEqual(['song-a', 'song-b']);
    expect(artistKeys).toEqual(['artist:ada']);
  });

  it('exposes pre-tokenized collection search through the source API', async () => {
    const store = new MemoryStore();
    const definition: CollectionDefinition<Song> = {
      ...songDefinition,
      searchIndexes: [
        {
          name: 'songs',
          prefix: 's:',
          options: { minKeywordLength: 1 },
          text: (song) => [song.title, song.artist, ...(song.tags ?? [])],
        },
      ],
    };
    const writer = new CollectionWriter(store, definition);

    await writer.put({ id: 'song-a', title: 'X', artist: 'Ada' }, cidFromSeed(9));

    const source = new CollectionSource(store, writer.manifest());
    expect(await source.search('songs', 'x', {
      fullMatch: true,
    })).toEqual([
      { id: 'song-a', cid: cidFromSeed(9), score: 1, exactMatches: 1, prefixDistance: 0 },
    ]);
    expect(await source.searchTerms('songs', ['X'], {
      limit: 10,
      scanLimit: 10,
      fullMatch: true,
    })).toEqual([
      { id: 'song-a', cid: cidFromSeed(9), score: 1, exactMatches: 1, prefixDistance: 0 },
    ]);
  });

  it('removes stale index entries when an item is replaced in batch', async () => {
    const store = new MemoryStore();
    const writer = new CollectionWriter(store, songDefinition);
    const original: Song = { id: 'song-a', title: 'Old Horizon', artist: 'Ada' };
    const replacement: Song = { id: 'song-a', title: 'New Horizon', artist: 'Ada' };

    await writer.put(original, cidFromSeed(10));
    await writer.batch([
      {
        type: 'put',
        item: replacement,
        previous: original,
        cid: cidFromSeed(11),
      },
    ]);

    const source = new CollectionSource(store, writer.manifest());
    expect(await source.get('song-a')).toEqual(cidFromSeed(11));
    expect(await source.search('songs', 'old')).toEqual([]);
    expect((await source.search('songs', 'new')).map((result) => result.id)).toEqual(['song-a']);
  });

  it('requires previous when replacing an indexed item', async () => {
    const store = new MemoryStore();
    const writer = new CollectionWriter(store, songDefinition);
    const original: Song = { id: 'song-a', title: 'Old Horizon', artist: 'Ada' };
    const replacement: Song = { id: 'song-a', title: 'New Horizon', artist: 'Bea' };

    await writer.put(original, cidFromSeed(15));

    await expect(writer.put(replacement, cidFromSeed(16))).rejects.toThrow(/requires options\.previous/);

    const source = new CollectionSource(store, writer.manifest());
    expect(await source.get('song-a')).toEqual(cidFromSeed(15));
    expect((await source.search('songs', 'old')).map((result) => result.id)).toEqual(['song-a']);
    expect(await source.search('songs', 'new')).toEqual([]);
  });

  it('allows by-id-only overwrites without a previous item snapshot', async () => {
    const store = new MemoryStore();
    const writer = new CollectionWriter(store, byIdOnlySongDefinition);

    await writer.put({ id: 'song-a', title: 'Old Horizon', artist: 'Ada' }, cidFromSeed(17));
    await writer.put({ id: 'song-a', title: 'New Horizon', artist: 'Bea' }, cidFromSeed(18));

    const source = new CollectionSource(store, writer.manifest());
    expect(await source.get('song-a')).toEqual(cidFromSeed(18));
    expect(await source.count()).toBe(1);
  });

  it('exposes an explicit replace helper for indexed updates', async () => {
    const store = new MemoryStore();
    const writer = new CollectionWriter(store, songDefinition);
    const original: Song = { id: 'song-a', title: 'Old Horizon', artist: 'Ada' };
    const replacement: Song = { id: 'song-a', title: 'New Horizon', artist: 'Bea' };

    await writer.put(original, cidFromSeed(19));
    await writer.replace(replacement, cidFromSeed(20), original);

    const source = new CollectionSource(store, writer.manifest());
    expect(await source.get('song-a')).toEqual(cidFromSeed(20));
    expect(await source.search('songs', 'old')).toEqual([]);
    expect((await source.search('songs', 'new')).map((result) => result.id)).toEqual(['song-a']);
  });

  it('reindexes from canonical entries and clears stale derived state', async () => {
    const store = new MemoryStore();
    const writer = new CollectionWriter(store, songDefinition);
    const original: Song = { id: 'song-a', title: 'Old Horizon', artist: 'Ada', tags: ['night'] };
    const replacement: Song = { id: 'song-a', title: 'New Horizon', artist: 'Bea', tags: ['day'] };
    const other: Song = { id: 'song-b', title: 'Sun Clock', artist: 'Bea', tags: ['ambient'] };

    await writer.put(original, cidFromSeed(12));
    await writer.reindex([
      { item: replacement, cid: cidFromSeed(13) },
      { item: other, cid: cidFromSeed(14) },
    ]);

    const source = new CollectionSource(store, writer.manifest());
    expect(await source.get('song-a')).toEqual(cidFromSeed(13));
    expect(await source.search('songs', 'old')).toEqual([]);
    expect((await source.search('songs', 'new')).map((result) => result.id)).toEqual(['song-a']);
    expect((await source.queryIndex('artist', { prefix: 'artist:ada' })).map((result) => result.key)).toEqual([]);
    expect((await source.queryIndex('artist', { prefix: 'artist:bea' })).map((result) => result.key)).toEqual([
      'artist:bea',
    ]);
  });

  it('federates search across multiple source manifests and dedupes by id', async () => {
    const store = new MemoryStore();
    const globalWriter = new CollectionWriter(store, {
      ...songDefinition,
      sourceId: 'global-catalog',
    });
    const selfWriter = new CollectionWriter(store, {
      ...songDefinition,
      sourceId: 'self-catalog',
    });

    await globalWriter.put({ id: 'shared-song', title: 'Starlight Echo', artist: 'Ada' }, cidFromSeed(20));
    await globalWriter.put({ id: 'global-only', title: 'Garden Static', artist: 'Bea' }, cidFromSeed(21));
    await selfWriter.put({ id: 'shared-song', title: 'Starlight Echo', artist: 'Ada' }, cidFromSeed(30));
    await selfWriter.put({ id: 'self-only', title: 'Starlight Ritual', artist: 'Ada' }, cidFromSeed(31));

    const results = await federatedSearch(store, [
      { manifest: globalWriter.manifest(), boost: 1 },
      { manifest: selfWriter.manifest(), boost: 2 },
    ], 'songs', 'starlight');

    expect(results.map((result) => result.id)).toEqual(['shared-song', 'self-only']);
    expect(results[0]?.sourceIds).toEqual(['global-catalog', 'self-catalog']);
    expect(results[0]?.bestSourceId).toBe('self-catalog');
    expect(results[0]?.score).toBeGreaterThan(results[1]?.score ?? 0);
  });

  it('supports schema defaults and migration helpers without adding heavy validation machinery', async () => {
    interface MigratingSong {
      id: string;
      title: string;
      artist: string;
      tags: string[];
    }

    const definition: CollectionDefinition<MigratingSong> = {
      sourceId: 'schema-catalog',
      schema: {
        version: 2,
        defaults: {
          artist: 'Unknown',
          tags: [],
        },
        migrate: (value, fromVersion) => {
          if (fromVersion !== 1 || !value || typeof value !== 'object') {
            throw new Error('unsupported migration');
          }
          const candidate = value as { id?: string; title?: string; creator?: string; tags?: string[] };
          return {
            id: `${candidate.id ?? ''}`,
            title: `${candidate.title ?? ''}`,
            artist: `${candidate.creator ?? ''}` || 'Unknown',
            tags: Array.isArray(candidate.tags) ? candidate.tags : [],
          };
        },
        normalize: (song) => ({
          ...song,
          title: song.title.trim(),
          artist: song.artist.trim() || 'Unknown',
          tags: [...new Set(song.tags.map((tag) => tag.trim()).filter(Boolean))],
        }),
        validate: (song) => {
          if (!song.id.trim()) {
            throw new Error('id required');
          }
        },
      },
      publishedSchema: {
        itemFormat: 'example/song@1',
        projectionFormat: 'example/song-index@1',
      },
      getId: (song) => song.id,
      keyIndexes: [
        {
          name: 'artist',
          keys: (song) => [`artist:${song.artist.toLowerCase()}`],
        },
      ],
      searchIndexes: [
        {
          name: 'songs',
          prefix: 's:',
          text: (song) => [song.title, song.artist, ...song.tags],
        },
      ],
    };

    const migrated = normalizeCollectionItem(definition, {
      id: 'song-c',
      title: '  Lantern Bloom  ',
      creator: ' Ada ',
      tags: ['ambient', 'ambient', '  night '],
    }, { fromVersion: 1 });

    expect(migrated).toEqual({
      id: 'song-c',
      title: 'Lantern Bloom',
      artist: 'Ada',
      tags: ['ambient', 'night'],
    });

    const store = new MemoryStore();
    const writer = new CollectionWriter(store, definition);
    await writer.put(migrated, cidFromSeed(40));

    const source = new CollectionSource(store, writer.manifest());
    expect(source.manifest.schemaVersion).toBe(2);
    expect(source.manifest.publishedSchema).toEqual({
      itemFormat: 'example/song@1',
      projectionFormat: 'example/song-index@1',
    });
    expect((await source.queryIndex('artist', { prefix: 'artist:ada' })).map((result) => result.key)).toEqual(['artist:ada']);
    expect((await source.search('songs', 'night')).map((result) => result.id)).toEqual(['song-c']);
  });

  it('serializes published collection manifest metadata in a shared root file format', async () => {
    const metadata = collectionManifestMetadataFromManifest({
      version: 1,
      sourceId: 'npub1test/audio',
      schemaVersion: 2,
      updatedAt: 0,
      itemCount: 1,
      byIdRoot: null,
      indexes: {},
      publishedSchema: {
        itemFormat: 'example/song@1',
        projectionFormat: 'example/song-index@1',
      },
    });

    expect(metadata).toEqual({
      version: 1,
      schemaVersion: 2,
      publishedSchema: {
        itemFormat: 'example/song@1',
        projectionFormat: 'example/song-index@1',
      },
    });
    expect(
      parseCollectionManifestMetadata(serializeCollectionManifestMetadata(metadata!)),
    ).toEqual(metadata);
  });

  it('supports shared search roots with derived entity targets', async () => {
    interface CatalogSong {
      id: string;
      title: string;
      artist: string;
      artistId: string;
      album: string;
      albumId: string;
    }

    const definition: CollectionDefinition<CatalogSong> = {
      sourceId: 'catalog',
      getId: (song) => song.id,
      searchIndexes: [
        {
          name: 'songs',
          rootName: 'catalog-search',
          prefix: 's:',
          text: (song) => [song.title, song.artist, song.album],
        },
        {
          name: 'artists',
          rootName: 'catalog-search',
          prefix: 'a:',
          entries: (song, context) => [{
            id: song.artistId,
            cid: context.writeContext?.artistCid as CID,
            text: song.artist,
          }],
        },
        {
          name: 'albums',
          rootName: 'catalog-search',
          prefix: 'l:',
          entries: (song, context) => [{
            id: song.albumId,
            cid: context.writeContext?.albumCid as CID,
            text: [song.album, song.artist],
          }],
        },
      ],
    };

    const store = new MemoryStore();
    const writer = new CollectionWriter(store, definition);
    await writer.put({
      id: 'song-1',
      title: 'Quiet Bloom',
      artist: 'Open Meridian',
      artistId: 'artist-1',
      album: 'Harbor Echo',
      albumId: 'album-1',
    }, cidFromSeed(50), {
      context: {
        artistCid: cidFromSeed(51),
        albumCid: cidFromSeed(52),
      },
    });

    const source = new CollectionSource(store, writer.manifest());
    expect(source.manifest.indexes.songs.root).toEqual(source.manifest.indexes.artists.root);
    expect(source.manifest.indexes.songs.root).toEqual(source.manifest.indexes.albums.root);
    expect((await source.search('songs', 'quiet')).map((result) => result.id)).toEqual(['song-1']);
    expect((await source.search('artists', 'open')).map((result) => result.id)).toEqual(['artist-1']);
    expect((await source.search('albums', 'harbor')).map((result) => result.id)).toEqual(['album-1']);
  });

  it('bulk reindex rebuilds shared search roots with derived entity targets', async () => {
    interface CatalogSong {
      id: string;
      title: string;
      artist: string;
      artistId: string;
      album: string;
      albumId: string;
    }

    const definition: CollectionDefinition<CatalogSong> = {
      sourceId: 'catalog',
      getId: (song) => song.id,
      keyIndexes: [
        {
          name: 'artist',
          keys: (song) => [song.artistId],
        },
      ],
      searchIndexes: [
        {
          name: 'songs',
          rootName: 'catalog-search',
          prefix: 's:',
          text: (song) => [song.title, song.artist, song.album],
        },
        {
          name: 'artists',
          rootName: 'catalog-search',
          prefix: 'a:',
          entries: (song, context) => [{
            id: song.artistId,
            cid: context.writeContext?.artistCid as CID,
            text: song.artist,
          }],
        },
        {
          name: 'albums',
          rootName: 'catalog-search',
          prefix: 'l:',
          entries: (song, context) => [{
            id: song.albumId,
            cid: context.writeContext?.albumCid as CID,
            text: [song.album, song.artist],
          }],
        },
      ],
    };

    const store = new MemoryStore();
    const writer = new CollectionWriter(store, definition);

    await writer.reindex([
      {
        item: {
          id: 'song-1',
          title: 'Quiet Bloom',
          artist: 'Open Meridian',
          artistId: 'artist-1',
          album: 'Harbor Echo',
          albumId: 'album-1',
        },
        cid: cidFromSeed(60),
        context: {
          artistCid: cidFromSeed(61),
          albumCid: cidFromSeed(62),
        },
      },
      {
        item: {
          id: 'song-2',
          title: 'Silver Static',
          artist: 'Night Circuit',
          artistId: 'artist-2',
          album: 'Glass Transit',
          albumId: 'album-2',
        },
        cid: cidFromSeed(63),
        context: {
          artistCid: cidFromSeed(64),
          albumCid: cidFromSeed(65),
        },
      },
    ]);

    const source = new CollectionSource(store, writer.manifest());
    expect(source.manifest.indexes.songs.root).toEqual(source.manifest.indexes.artists.root);
    expect(source.manifest.indexes.songs.root).toEqual(source.manifest.indexes.albums.root);
    expect((await source.search('songs', 'quiet')).map((result) => result.id)).toEqual(['song-1']);
    expect((await source.search('songs', 'silver')).map((result) => result.id)).toEqual(['song-2']);
    expect((await source.search('artists', 'open')).map((result) => result.id)).toEqual(['artist-1']);
    expect((await source.search('artists', 'night')).map((result) => result.id)).toEqual(['artist-2']);
    expect((await source.search('albums', 'harbor')).map((result) => result.id)).toEqual(['album-1']);
    expect((await source.queryIndex('artist', { prefix: 'artist-2' })).map((result) => result.key)).toEqual(['artist-2']);
  });
});
