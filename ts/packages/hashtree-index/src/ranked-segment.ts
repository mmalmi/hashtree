import {
  HashTree,
  LinkType,
  type CID,
  type TreeEntry,
} from '@hashtree/core';
import { decodeRankedManifest } from './ranked-schema.js';
import {
  decodeRankedTopKManifest,
  encodeRankedTopKManifest,
  type RankedTopKManifest,
} from './ranked-top-k.js';
import type { RankedSearchSegmentManifest } from './ranked-types.js';

const encoder = new TextEncoder();
const decoder = new TextDecoder();

export type RankedSegmentRoots = {
  postings: CID | null;
  terms: CID | null;
  documents: CID | null;
  values: CID | null;
  topK: CID | null;
};

export async function writeRankedSegment(
  tree: HashTree,
  manifest: RankedSearchSegmentManifest,
  roots: RankedSegmentRoots,
  topKManifest: RankedTopKManifest | null,
): Promise<CID> {
  const manifestFile = await tree.putFile(encoder.encode(JSON.stringify(manifest)));
  const entries: TreeEntry[] = [
    { name: 'manifest.json', cid: manifestFile.cid, size: manifestFile.size, type: LinkType.File },
  ];
  if (topKManifest) {
    const topKFile = await tree.putFile(encoder.encode(encodeRankedTopKManifest(topKManifest)));
    entries.push({ name: 'top-k.json', cid: topKFile.cid, size: topKFile.size, type: LinkType.File });
  }
  const linkedRoots = {
    documents: roots.documents,
    postings: roots.postings,
    terms: roots.terms,
    'top-k-roots': roots.topK,
    values: roots.values,
  };
  for (const [name, cid] of Object.entries(linkedRoots).sort(([left], [right]) =>
    compareStrings(left, right))) {
    if (cid) entries.push({ name, cid, size: 0, type: LinkType.Dir });
  }
  entries.sort((left, right) => compareStrings(left.name, right.name));
  return (await tree.putDirectory(entries)).cid;
}

export async function readRankedSegment(
  tree: HashTree,
  root: CID,
): Promise<{
  manifest: RankedSearchSegmentManifest;
  roots: RankedSegmentRoots;
  topKManifest: RankedTopKManifest | null;
}> {
  const entries = await tree.listDirectory(root);
  const manifestEntry = entries.find((entry) => entry.name === 'manifest.json');
  if (!manifestEntry) throw new Error('Missing ranked search segment manifest');
  const manifestBytes = await tree.readFile(manifestEntry.cid);
  if (!manifestBytes) throw new Error('Unreadable ranked search segment manifest');
  const topKEntry = entries.find((entry) => entry.name === 'top-k.json');
  const topKBytes = topKEntry ? await tree.readFile(topKEntry.cid) : null;
  if (topKEntry && !topKBytes) throw new Error('Unreadable ranked top-k manifest');
  const link = (name: string): CID | null =>
    entries.find((entry) => entry.name === name)?.cid ?? null;
  const topKRoot = link('top-k-roots');
  if ((topKBytes !== null) !== (topKRoot !== null)) {
    throw new Error('Incomplete ranked top-k acceleration');
  }
  return {
    manifest: decodeRankedManifest(decoder.decode(manifestBytes)),
    topKManifest: topKBytes ? decodeRankedTopKManifest(decoder.decode(topKBytes)) : null,
    roots: {
      postings: link('postings'),
      terms: link('terms'),
      documents: link('documents'),
      values: link('values'),
      topK: topKRoot,
    },
  };
}

function compareStrings(left: string, right: string): number {
  return left < right ? -1 : left > right ? 1 : 0;
}
