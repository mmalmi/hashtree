import {
  HashTree,
  LinkType,
  type CID,
  type TreeEntry,
} from '@hashtree/core';
import { decodeRankedManifest } from './ranked-schema.js';
import type { RankedSearchSegmentManifest } from './ranked-types.js';

const encoder = new TextEncoder();
const decoder = new TextDecoder();

export type RankedSegmentRoots = {
  postings: CID | null;
  terms: CID | null;
  documents: CID | null;
  values: CID | null;
};

export async function writeRankedSegment(
  tree: HashTree,
  manifest: RankedSearchSegmentManifest,
  roots: RankedSegmentRoots,
): Promise<CID> {
  const manifestFile = await tree.putFile(encoder.encode(JSON.stringify(manifest)));
  const entries: TreeEntry[] = [
    { name: 'manifest.json', cid: manifestFile.cid, size: manifestFile.size, type: LinkType.File },
  ];
  for (const [name, cid] of Object.entries(roots).sort(([left], [right]) =>
    compareStrings(left, right))) {
    if (cid) entries.push({ name, cid, size: 0, type: LinkType.Dir });
  }
  entries.sort((left, right) => compareStrings(left.name, right.name));
  return (await tree.putDirectory(entries)).cid;
}

export async function readRankedSegment(
  tree: HashTree,
  root: CID,
): Promise<{ manifest: RankedSearchSegmentManifest; roots: RankedSegmentRoots }> {
  const entries = await tree.listDirectory(root);
  const manifestEntry = entries.find((entry) => entry.name === 'manifest.json');
  if (!manifestEntry) throw new Error('Missing ranked search segment manifest');
  const manifestBytes = await tree.readFile(manifestEntry.cid);
  if (!manifestBytes) throw new Error('Unreadable ranked search segment manifest');
  const link = (name: string): CID | null =>
    entries.find((entry) => entry.name === name)?.cid ?? null;
  return {
    manifest: decodeRankedManifest(decoder.decode(manifestBytes)),
    roots: {
      postings: link('postings'),
      terms: link('terms'),
      documents: link('documents'),
      values: link('values'),
    },
  };
}

function compareStrings(left: string, right: string): number {
  return left < right ? -1 : left > right ? 1 : 0;
}
