/**
 * Release store and helpers for git repositories.
 *
 * Releases are stored at: npub/releases/<repoPath>
 * Each release is a directory containing release.json, notes.md, and assets/.
 */
import { writable, type Readable } from 'svelte/store';
import { LinkType, type CID, type TreeEntry, type TreeVisibility } from 'hashtree';
import { getTree, decodeAsText } from '../store';
import { waitForTreeRoot } from './treeRoot';
import { onCacheUpdate } from '../treeRootCache';
import { saveHashtree } from '../nostr';
import { getErrorMessage } from '../utils/errorMessage';

const RELEASES_PREFIX = 'releases';

export interface ReleaseSummary {
  id: string;
  title: string;
  tag?: string;
  created_at: number;
  published_at?: number;
  draft?: boolean;
  prerelease?: boolean;
  commit?: string;
}

export interface ReleaseAsset {
  name: string;
  path: string;
  size: number;
  cid?: CID;
}

export interface ReleaseDetail extends ReleaseSummary {
  notes?: string;
  notesFile?: string;
  assets: ReleaseAsset[];
}

interface ReleaseAssetMeta {
  name: string;
  path: string;
  size: number;
}

interface ReleaseRecord extends ReleaseSummary {
  notes_file?: string;
  assets?: ReleaseAssetMeta[];
}

export interface ReleasesState {
  items: ReleaseSummary[];
  loading: boolean;
  error: string | null;
}

export interface SaveReleaseOptions {
  npub: string;
  repoPath: string;
  title: string;
  tag?: string;
  commit?: string;
  notes?: string;
  draft?: boolean;
  prerelease?: boolean;
  assets?: File[];
  existingAssets?: ReleaseAsset[];
  existingIds?: string[];
  releaseId?: string;
  createdAt?: number;
  publishedAt?: number;
  visibility?: TreeVisibility;
  linkKey?: string;
}

function normalizeRepoPath(repoPath: string): string {
  return repoPath.replace(/^\/+/, '').replace(/\/+$/, '');
}

export function buildReleaseTreeName(repoPath: string): string {
  const clean = normalizeRepoPath(repoPath);
  return clean ? `${RELEASES_PREFIX}/${clean}` : RELEASES_PREFIX;
}

export function sanitizeReleaseId(input: string): string {
  const trimmed = input.trim();
  if (!trimmed) return '';
  const cleaned = trimmed
    .replace(/[\\/]/g, '-')
    .replace(/[^a-zA-Z0-9._-]+/g, '-')
    .replace(/-+/g, '-')
    .replace(/^[-.]+|[-.]+$/g, '');
  return cleaned.slice(0, 80);
}

function ensureUniqueReleaseId(base: string, existing: Set<string>): string {
  if (!existing.has(base)) return base;
  const suffix = Date.now().toString(36).slice(-6);
  let candidate = `${base}-${suffix}`;
  if (!existing.has(candidate)) return candidate;
  let i = 2;
  while (existing.has(`${base}-${i}`)) i += 1;
  return `${base}-${i}`;
}

function getReleaseSortTimestamp(release: ReleaseSummary): number {
  return release.published_at ?? release.created_at ?? 0;
}

function parseReleaseMeta(meta: TreeEntry['meta'] | undefined, fallbackId: string): ReleaseSummary | null {
  if (!meta || typeof meta !== 'object') return null;
  const metaRecord = (meta as { release?: unknown }).release ?? meta;
  if (!metaRecord || typeof metaRecord !== 'object') return null;

  const record = metaRecord as Record<string, unknown>;
  const title = typeof record.title === 'string' ? record.title : '';
  const tag = typeof record.tag === 'string' ? record.tag : undefined;
  const commit = typeof record.commit === 'string' ? record.commit : undefined;
  const createdAt = typeof record.created_at === 'number' ? record.created_at : undefined;
  const publishedAt = typeof record.published_at === 'number' ? record.published_at : undefined;
  const draft = typeof record.draft === 'boolean' ? record.draft : undefined;
  const prerelease = typeof record.prerelease === 'boolean' ? record.prerelease : undefined;
  const id = typeof record.id === 'string' ? record.id : fallbackId;

  if (!title && !tag && !createdAt && !publishedAt) return null;

  return {
    id,
    title: title || tag || fallbackId,
    tag,
    commit,
    created_at: createdAt ?? publishedAt ?? 0,
    published_at: publishedAt,
    draft,
    prerelease,
  };
}

function toReleaseSummary(record: ReleaseRecord, fallbackId: string): ReleaseSummary {
  return {
    id: record.id || fallbackId,
    title: record.title || record.tag || fallbackId,
    tag: record.tag,
    commit: record.commit,
    created_at: record.created_at ?? record.published_at ?? 0,
    published_at: record.published_at,
    draft: record.draft,
    prerelease: record.prerelease,
  };
}

async function readReleaseRecord(tree: ReturnType<typeof getTree>, releaseCid: CID): Promise<ReleaseRecord | null> {
  const result = await tree.resolvePath(releaseCid, 'release.json');
  if (!result?.cid) return null;
  const data = await tree.readFile(result.cid);
  if (!data) return null;
  try {
    const text = new TextDecoder().decode(data);
    return JSON.parse(text) as ReleaseRecord;
  } catch {
    return null;
  }
}

async function readReleaseNotes(
  tree: ReturnType<typeof getTree>,
  releaseCid: CID,
  notesFile: string
): Promise<string | null> {
  const result = await tree.resolvePath(releaseCid, notesFile);
  if (!result?.cid) return null;
  const data = await tree.readFile(result.cid);
  if (!data) return null;
  return decodeAsText(data) ?? new TextDecoder().decode(data);
}

async function listReleaseAssets(tree: ReturnType<typeof getTree>, releaseCid: CID): Promise<ReleaseAsset[]> {
  const assetsEntry = await tree.resolvePath(releaseCid, 'assets');
  if (!assetsEntry?.cid || assetsEntry.type !== LinkType.Dir) return [];
  const entries = await tree.listDirectory(assetsEntry.cid);
  return entries
    .filter(entry => entry.type !== LinkType.Dir)
    .map(entry => ({
      name: entry.name,
      path: `assets/${entry.name}`,
      size: entry.size ?? 0,
      cid: entry.cid,
    }));
}

async function listReleaseEntries(npub: string, repoPath: string): Promise<ReleaseSummary[]> {
  const treeName = buildReleaseTreeName(repoPath);
  const rootCid = await waitForTreeRoot(npub, treeName, 8000);
  if (!rootCid) return [];

  const tree = getTree();
  const entries = await tree.listDirectory(rootCid);
  const releases: ReleaseSummary[] = [];

  for (const entry of entries) {
    if (entry.type !== LinkType.Dir || !entry.cid) continue;
    const metaSummary = parseReleaseMeta(entry.meta, entry.name);
    if (metaSummary) {
      releases.push(metaSummary);
      continue;
    }

    const record = await readReleaseRecord(tree, entry.cid);
    if (record) {
      releases.push(toReleaseSummary(record, entry.name));
    }
  }

  return releases.sort((a, b) => getReleaseSortTimestamp(b) - getReleaseSortTimestamp(a));
}

export async function fetchReleaseDetail(
  npub: string,
  repoPath: string,
  releaseId: string
): Promise<ReleaseDetail | null> {
  const treeName = buildReleaseTreeName(repoPath);
  const rootCid = await waitForTreeRoot(npub, treeName, 8000);
  if (!rootCid) return null;

  const tree = getTree();
  const entries = await tree.listDirectory(rootCid);
  const entry = entries.find(item => item.name === releaseId);
  if (!entry?.cid || entry.type !== LinkType.Dir) return null;

  const record = await readReleaseRecord(tree, entry.cid);
  const metaSummary = parseReleaseMeta(entry.meta, entry.name);
  const summary = record ? toReleaseSummary(record, entry.name) : (metaSummary ?? {
    id: entry.name,
    title: entry.name,
    created_at: 0,
  });

  const notesFile = record?.notes_file || 'notes.md';
  const notes = await readReleaseNotes(tree, entry.cid, notesFile);
  const assets = await listReleaseAssets(tree, entry.cid);

  return {
    ...summary,
    notes: notes ?? undefined,
    notesFile: notes ? notesFile : undefined,
    assets,
  };
}

export function createReleasesStore(
  npub: string | null,
  repoPath: string | null
): Readable<ReleasesState> & { refresh: () => Promise<void> } {
  const { subscribe, set, update } = writable<ReleasesState>({
    items: [],
    loading: true,
    error: null,
  });

  let releaseTreeName: string | null = repoPath ? buildReleaseTreeName(repoPath) : null;

  async function refresh(): Promise<void> {
    if (!npub || !repoPath) {
      set({ items: [], loading: false, error: null });
      return;
    }

    update(state => ({ ...state, loading: true, error: null }));

    try {
      const items = await listReleaseEntries(npub, repoPath);
      set({ items, loading: false, error: null });
    } catch (err) {
      set({ items: [], loading: false, error: getErrorMessage(err) });
    }
  }

  if (npub && repoPath) {
    refresh();
  } else {
    set({ items: [], loading: false, error: null });
  }

  if (npub && releaseTreeName) {
    onCacheUpdate((owner, treeName) => {
      if (owner === npub && treeName === releaseTreeName) {
        refresh();
      }
    });
  }

  return { subscribe, refresh };
}

export async function saveRelease(options: SaveReleaseOptions): Promise<ReleaseSummary | null> {
  const title = options.title.trim();
  if (!title) return null;
  if (options.visibility === 'link-visible' && !options.linkKey) {
    throw new Error('Missing link key for link-visible release tree');
  }

  const treeName = buildReleaseTreeName(options.repoPath);
  const tree = getTree();
  const rootCid = await waitForTreeRoot(options.npub, treeName, 5000);
  const existingIds = new Set(options.existingIds ?? []);
  if (!options.releaseId && rootCid) {
    const entries = await tree.listDirectory(rootCid);
    entries.forEach(entry => existingIds.add(entry.name));
  }

  let releaseId = options.releaseId?.trim() ?? '';
  if (!releaseId) {
    const base = sanitizeReleaseId(options.tag ?? title) || `release-${Date.now()}`;
    releaseId = ensureUniqueReleaseId(base, existingIds);
  }

  const now = Math.floor(Date.now() / 1000);
  const created_at = options.createdAt ?? now;
  const published_at = options.draft ? undefined : (options.publishedAt ?? now);

  const notes = options.notes?.trim() ?? '';
  const notesFile = notes ? 'notes.md' : undefined;
  let notesEntry: { cid: CID; size: number } | null = null;
  if (notesFile) {
    const notesBytes = new TextEncoder().encode(notes);
    notesEntry = await tree.putFile(notesBytes);
  }

  const assetMap = new Map<string, { cid: CID; size: number }>();
  const existingAssets = options.existingAssets ?? [];
  for (const asset of existingAssets) {
    if (asset.cid) {
      assetMap.set(asset.name, { cid: asset.cid, size: asset.size });
    }
  }

  for (const file of options.assets ?? []) {
    const data = new Uint8Array(await file.arrayBuffer());
    const result = await tree.putFile(data);
    assetMap.set(file.name, { cid: result.cid, size: result.size });
  }

  let assetsDirEntry: { cid: CID } | null = null;
  const assetMeta: ReleaseAssetMeta[] = [];
  if (assetMap.size > 0) {
    const assetEntries = Array.from(assetMap.entries()).map(([name, info]) => {
      assetMeta.push({ name, path: `assets/${name}`, size: info.size });
      return { name, cid: info.cid, size: info.size, type: LinkType.File };
    });
    const assetsDir = await tree.putDirectory(assetEntries);
    assetsDirEntry = { cid: assetsDir.cid };
  }

  const record: ReleaseRecord = {
    id: releaseId,
    title,
    tag: options.tag?.trim() || undefined,
    commit: options.commit?.trim() || undefined,
    created_at,
    published_at,
    draft: options.draft ?? false,
    prerelease: options.prerelease ?? false,
    notes_file: notesFile,
    assets: assetMeta.length > 0 ? assetMeta : undefined,
  };

  const recordBytes = new TextEncoder().encode(JSON.stringify(record, null, 2));
  const recordEntry = await tree.putFile(recordBytes);

  const releaseEntries = [
    { name: 'release.json', cid: recordEntry.cid, size: recordEntry.size, type: LinkType.File },
  ];
  if (notesEntry) {
    releaseEntries.push({ name: notesFile!, cid: notesEntry.cid, size: notesEntry.size, type: LinkType.File });
  }
  if (assetsDirEntry) {
    releaseEntries.push({ name: 'assets', cid: assetsDirEntry.cid, size: 0, type: LinkType.Dir });
  }

  const releaseDir = await tree.putDirectory(releaseEntries);
  const meta = {
    release: toReleaseSummary(record, releaseId),
  };

  let newRootCid: CID;
  if (rootCid) {
    newRootCid = await tree.setEntry(rootCid, [], releaseId, releaseDir.cid, 0, LinkType.Dir, meta);
  } else {
    const root = await tree.putDirectory([
      { name: releaseId, cid: releaseDir.cid, size: 0, type: LinkType.Dir, meta },
    ]);
    newRootCid = root.cid;
  }

  const visibility = options.visibility ?? 'public';
  const result = await saveHashtree(treeName, newRootCid, { visibility, linkKey: options.linkKey });
  if (!result.success) {
    throw new Error('Failed to publish release tree');
  }

  return toReleaseSummary(record, releaseId);
}

export async function deleteRelease(
  npub: string,
  repoPath: string,
  releaseId: string,
  visibility: TreeVisibility = 'public',
  linkKey?: string
): Promise<boolean> {
  if (visibility === 'link-visible' && !linkKey) return false;
  const treeName = buildReleaseTreeName(repoPath);
  const rootCid = await waitForTreeRoot(npub, treeName, 5000);
  if (!rootCid) return false;

  const tree = getTree();
  const newRootCid = await tree.removeEntry(rootCid, [], releaseId);
  const result = await saveHashtree(treeName, newRootCid, { visibility, linkKey });
  return result.success;
}
