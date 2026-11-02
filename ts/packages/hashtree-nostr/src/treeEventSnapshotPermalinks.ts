import { nhashDecode } from '@hashtree/core';

const NHASH_PATTERN = /^nhash1[a-z0-9]+$/;
const LINK_KEY_PATTERN = /^(?:[a-f0-9]{2}){32,33}$/i;

export interface TreeEventSnapshotPermalink {
  snapshotNhash: string;
  path: string[];
  linkKey?: string;
}

export interface BuildTreeEventSnapshotPermalinkOptions {
  prefix?: string;
}

function safeDecode(value: string): string {
  try {
    return decodeURIComponent(value);
  } catch {
    return value;
  }
}

function extractPermalinkCandidate(input: string | URL): string {
  const raw = `${input instanceof URL ? input.toString() : input}`.trim();
  if (!raw) {
    return '';
  }

  try {
    const parsed = input instanceof URL ? input : new URL(raw);
    if (parsed.hash.startsWith('#/')) {
      return parsed.hash.slice(1);
    }
    return `/${parsed.host}${parsed.pathname}${parsed.search}`;
  } catch {
    if (raw.startsWith('#/')) {
      return raw.slice(1);
    }
    return raw;
  }
}

function normalizeSnapshotNhash(value: string): string | null {
  const trimmed = value.trim();
  if (!NHASH_PATTERN.test(trimmed)) {
    return null;
  }

  try {
    const decoded = nhashDecode(trimmed);
    if (decoded.key) {
      return null;
    }
    return trimmed;
  } catch {
    return null;
  }
}

export function normalizeTreeEventSnapshotLinkKey(value?: string | null): string | null {
  const trimmed = value?.trim();
  if (!trimmed) {
    return null;
  }
  if (!LINK_KEY_PATTERN.test(trimmed)) {
    return null;
  }
  return trimmed.toLowerCase();
}

function normalizeTreeEventSnapshotPermalink(
  value: TreeEventSnapshotPermalink,
): TreeEventSnapshotPermalink {
  const snapshotNhash = normalizeSnapshotNhash(value.snapshotNhash);
  if (!snapshotNhash) {
    throw new Error(`Invalid tree event snapshot nhash: ${value.snapshotNhash}`);
  }

  const linkKey = normalizeTreeEventSnapshotLinkKey(value.linkKey);
  if (value.linkKey?.trim() && !linkKey) {
    throw new Error('Invalid tree event snapshot link key');
  }

  return {
    snapshotNhash,
    path: value.path.filter(Boolean),
    ...(linkKey ? { linkKey } : {}),
  };
}

export function buildTreeEventSnapshotPermalink(
  value: TreeEventSnapshotPermalink,
  options: BuildTreeEventSnapshotPermalinkOptions = {},
): string {
  const normalized = normalizeTreeEventSnapshotPermalink(value);
  const encodedPath = normalized.path.map((part) => encodeURIComponent(part)).join('/');
  const query = new URLSearchParams();
  query.set('snapshot', '1');
  if (normalized.linkKey) {
    query.set('k', normalized.linkKey);
  }
  const prefix = options.prefix ?? '';
  return `${prefix}${normalized.snapshotNhash}${encodedPath ? `/${encodedPath}` : ''}?${query.toString()}`;
}

export function parseTreeEventSnapshotPermalink(input: string | URL): TreeEventSnapshotPermalink | null {
  const candidate = extractPermalinkCandidate(input);
  if (!candidate) {
    return null;
  }

  const [pathPart, queryPart = ''] = candidate.split('?', 2);
  const parts = pathPart
    .replace(/^\/+/, '')
    .split('/')
    .filter(Boolean)
    .map(safeDecode);

  if (parts[0] === 'nhash') {
    parts.shift();
  }

  const snapshotNhash = parts.shift();
  if (!snapshotNhash) {
    return null;
  }

  const normalizedSnapshotNhash = normalizeSnapshotNhash(snapshotNhash);
  if (!normalizedSnapshotNhash) {
    return null;
  }

  const params = new URLSearchParams(queryPart);
  if (params.get('snapshot')?.trim() !== '1') {
    return null;
  }

  const rawLinkKey = params.get('k');
  const linkKey = normalizeTreeEventSnapshotLinkKey(rawLinkKey);
  if (rawLinkKey?.trim() && !linkKey) {
    return null;
  }

  return {
    snapshotNhash: normalizedSnapshotNhash,
    path: parts,
    ...(linkKey ? { linkKey } : {}),
  };
}
