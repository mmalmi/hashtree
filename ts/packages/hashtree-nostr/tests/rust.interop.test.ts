import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { tmpdir } from 'node:os';
import { beforeAll, describe, expect, it } from 'vitest';
import { fromHex, HashTree, MemoryStore, type CID } from '@hashtree/core';
import {
  COLLECTION_MANIFEST_METADATA_FILE,
  MANIFEST_BY_AUTHOR_TIME,
  MANIFEST_BY_TAG,
  MANIFEST_PARAMETERIZED_REPLACEABLE,
  MANIFEST_REPLACEABLE,
  NostrEventStore,
  parameterizedReplaceableKey,
  replaceableKey,
  tagPrefix,
  type StoredNostrEvent,
} from '../src/index.js';

interface SerializedCid {
  hash: string;
  key?: string;
}

interface RustNostrFixture {
  root: SerializedCid;
  events: StoredNostrEvent[];
  blocks: Record<string, string>;
}

function cidFromFixture(cid: SerializedCid): CID {
  return {
    hash: fromHex(cid.hash),
    key: cid.key ? fromHex(cid.key) : undefined,
  };
}

async function storeFromFixtureBlocks(blocks: Record<string, string>): Promise<MemoryStore> {
  const store = new MemoryStore();
  for (const [hashHex, dataHex] of Object.entries(blocks)) {
    await store.put(fromHex(hashHex), fromHex(dataHex));
  }
  return store;
}

describe('Rust Nostr interop', () => {
  let fixture: RustNostrFixture;

  beforeAll(() => {
    const repoRoot = path.resolve(__dirname, '../../../..');
    const cargoRoot = path.join(repoRoot, 'rust');
    const outDir = mkdtempSync(path.join(tmpdir(), 'htree-nostr-interop-'));
    const outFile = path.join(outDir, 'nostr-event-store-fixture.json');

    execFileSync(
      'cargo',
      ['run', '-q', '-p', 'hashtree-nostr', '--bin', 'nostr-event-store-fixture', '--', outFile],
      { cwd: cargoRoot, stdio: 'inherit' },
    );

    fixture = JSON.parse(readFileSync(outFile, 'utf8')) as RustNostrFixture;
  }, 120000);

  it('matches rust-built event roots and direct collection indexes in typescript', async () => {
    const mainAuthor = 'a'.repeat(64);
    const otherAuthor = 'b'.repeat(64);
    const parameterizedAuthor = 'c'.repeat(64);
    const rustRoot = cidFromFixture(fixture.root);

    const older = fixture.events.find((event) => event.pubkey === mainAuthor && event.kind === 1 && event.content === 'older');
    const newer = fixture.events.find((event) => event.pubkey === mainAuthor && event.kind === 1 && event.content === 'newer');
    const latestProfile = fixture.events.find((event) => event.pubkey === mainAuthor && event.kind === 0 && event.content.includes('latest'));
    const staleProfile = fixture.events.find((event) => event.pubkey === mainAuthor && event.kind === 0 && event.content.includes('stale'));
    const tagged = fixture.events.find((event) => event.pubkey === mainAuthor && event.content === 'tagged');
    const other = fixture.events.find((event) => event.pubkey === otherAuthor && event.content === 'other');
    const drafts = fixture.events.filter((event) => event.pubkey === parameterizedAuthor && event.kind === 30_023);
    const parameterizedWinner = [...drafts].sort((left, right) => (
      right.created_at - left.created_at || left.id.localeCompare(right.id)
    ))[0];

    expect(older).toBeDefined();
    expect(newer).toBeDefined();
    expect(latestProfile).toBeDefined();
    expect(staleProfile).toBeDefined();
    expect(tagged).toBeDefined();
    expect(other).toBeDefined();
    expect(parameterizedWinner).toBeDefined();

    const tsBacking = new MemoryStore();
    const tsStore = new NostrEventStore(tsBacking);
    const tsRoot = await tsStore.build(null, fixture.events);

    expect(tsRoot).not.toBeNull();
    expect(tsRoot).toEqual(rustRoot);

    const rustBacking = await storeFromFixtureBlocks(fixture.blocks);
    const rustStore = new NostrEventStore(rustBacking);
    const rustCollection = await rustStore.getCollectionSource(rustRoot);

    expect(await tsStore.getManifest(tsRoot)).toEqual(await rustStore.getManifest(rustRoot));

    const tsTree = new HashTree({ store: tsBacking });
    const rustTree = new HashTree({ store: rustBacking });
    const tsMetadataEntry = (await tsTree.listDirectory(tsRoot!)).find(
      (entry) => entry.name === COLLECTION_MANIFEST_METADATA_FILE,
    );
    const rustMetadataEntry = (await rustTree.listDirectory(rustRoot)).find(
      (entry) => entry.name === COLLECTION_MANIFEST_METADATA_FILE,
    );
    expect(tsMetadataEntry).toBeDefined();
    expect(rustMetadataEntry).toBeDefined();
    expect(
      JSON.parse(new TextDecoder().decode((await tsTree.readFile(tsMetadataEntry!.cid))!)),
    ).toEqual(
      JSON.parse(new TextDecoder().decode((await rustTree.readFile(rustMetadataEntry!.cid))!)),
    );

    await expect(rustStore.getById(rustRoot, newer!.id)).resolves.toEqual(newer);
    await expect(rustStore.getById(rustRoot, staleProfile!.id)).resolves.toBeNull();
    await expect(rustStore.listByAuthor(rustRoot, mainAuthor)).resolves.toEqual([
      tagged!,
      latestProfile!,
      newer!,
      older!,
    ]);
    await expect(rustStore.listByKind(rustRoot, 1)).resolves.toEqual([
      tagged!,
      other!,
      newer!,
      older!,
    ]);
    await expect(rustStore.listByTag(rustRoot, 't', 'nostr')).resolves.toEqual([tagged!]);
    await expect(rustStore.getReplaceable(rustRoot, mainAuthor, 0)).resolves.toEqual(latestProfile);
    await expect(
      rustStore.getParameterizedReplaceable(rustRoot, parameterizedAuthor, 30_023, 'article-1'),
    ).resolves.toEqual(parameterizedWinner);

    const latestProfileCid = await rustCollection.get(latestProfile!.id);
    const taggedCid = await rustCollection.get(tagged!.id);

    expect(latestProfileCid).not.toBeNull();
    expect(taggedCid).not.toBeNull();
    expect(await rustCollection.get(staleProfile!.id)).toBeNull();
    expect(await rustCollection.getIndexLink(
      MANIFEST_REPLACEABLE,
      replaceableKey(mainAuthor, 0),
    )).toEqual(latestProfileCid);
    expect(await rustCollection.getIndexLink(
      MANIFEST_PARAMETERIZED_REPLACEABLE,
      parameterizedReplaceableKey(parameterizedAuthor, 30_023, 'article-1'),
    )).toEqual(await rustCollection.get(parameterizedWinner!.id));
    expect((await rustCollection.queryIndex(MANIFEST_BY_AUTHOR_TIME, {
      prefix: `${mainAuthor}:`,
    })).length).toBe(4);
    expect((await rustCollection.queryIndex(MANIFEST_BY_TAG, {
      prefix: tagPrefix('t', 'nostr'),
    })).map((entry) => entry.cid)).toEqual([taggedCid]);
  });
});
