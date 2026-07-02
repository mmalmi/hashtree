import {
  canUseInjectedHtreeServerUrl,
  resolveRuntimeHtreeBaseUrl,
  type HtreeRuntimeWindowLike,
} from './runtime.js';

export type ParsedHtreeUrl =
  | { kind: 'mutable'; npub: string; treeName: string; path: string }
  | { kind: 'immutable'; nhash: string; path: string };

export type MutableHtreeRequestStyle = 'htree' | 'gateway';

export interface ResolveHtreeRequestUrlOptions {
  windowLike?: HtreeRuntimeWindowLike;
  fallbackBaseUrl?: string | null;
  baseUrl?: string | null;
  mutableStyle?: MutableHtreeRequestStyle;
}

function safeDecodePathSegment(segment: string): string {
  try {
    return decodeURIComponent(segment);
  } catch {
    return segment;
  }
}

function encodePath(path: string): string {
  return path
    .split('/')
    .filter(Boolean)
    .map((segment) => encodeURIComponent(segment))
    .join('/');
}

function stripQueryAndHash(path: string): string {
  const hashIndex = path.indexOf('#');
  const pathWithoutHash = hashIndex === -1 ? path : path.slice(0, hashIndex);
  const queryIndex = pathWithoutHash.indexOf('?');
  return queryIndex === -1 ? pathWithoutHash : pathWithoutHash.slice(0, queryIndex);
}

function normalizeRelativePath(path: string): string {
  return stripQueryAndHash(path).replace(/^\/+/, '');
}

function isLoopbackHttpBaseUrl(baseUrl: string): boolean {
  try {
    const parsed = new URL(baseUrl);
    const hostname = parsed.hostname.toLowerCase();
    return (parsed.protocol === 'http:' || parsed.protocol === 'https:')
      && (hostname === '127.0.0.1' || hostname === 'localhost');
  } catch {
    return false;
  }
}

function resolveMutableRequestStyle(
  windowLike: HtreeRuntimeWindowLike | undefined,
  baseUrl: string,
  explicitStyle?: MutableHtreeRequestStyle,
): MutableHtreeRequestStyle {
  if (explicitStyle) return explicitStyle;
  if (!baseUrl) return 'htree';
  if (canUseInjectedHtreeServerUrl(windowLike)) return 'htree';
  if (isLoopbackHttpBaseUrl(baseUrl)) return 'htree';
  return 'gateway';
}

export function parseHtreeUrl(input: string): ParsedHtreeUrl | null {
  const trimmed = input.trim();
  if (!trimmed) return null;

  if (trimmed.startsWith('htree://')) {
    const withoutScheme = trimmed.slice('htree://'.length);
    const firstSlash = withoutScheme.indexOf('/');
    const head = firstSlash === -1 ? withoutScheme : withoutScheme.slice(0, firstSlash);
    const tail = firstSlash === -1 ? '' : withoutScheme.slice(firstSlash + 1);
    const rawSegments = normalizeRelativePath(tail).split('/').filter(Boolean);

    if (head.startsWith('npub1')) {
      const [rawTreeName = '', ...rawPathSegments] = rawSegments;
      const treeName = safeDecodePathSegment(rawTreeName);
      if (!treeName) {
        return null;
      }
      return {
        kind: 'mutable',
        npub: head,
        treeName,
        path: rawPathSegments.map(safeDecodePathSegment).join('/'),
      };
    }

    if (head.startsWith('nhash1')) {
      return {
        kind: 'immutable',
        nhash: head,
        path: rawSegments.map(safeDecodePathSegment).join('/'),
      };
    }

    return null;
  }

  if (trimmed.startsWith('nhash1')) {
    return {
      kind: 'immutable',
      nhash: stripQueryAndHash(trimmed),
      path: '',
    };
  }

  return null;
}

export function buildHtreeRequestPath(
  input: string | ParsedHtreeUrl,
  mutableStyle: MutableHtreeRequestStyle = 'htree',
): string | null {
  if (typeof input === 'string') {
    const trimmed = input.trim();
    if (!trimmed) return null;
    if (trimmed.startsWith('/htree/')) {
      return trimmed;
    }
  }

  const parsed = typeof input === 'string' ? parseHtreeUrl(input) : input;
  if (!parsed) return typeof input === 'string' ? input.trim() : null;

  if (parsed.kind === 'mutable') {
    const encodedTreeName = encodeURIComponent(parsed.treeName);
    const encodedPath = encodePath(parsed.path);
    const prefix = mutableStyle === 'htree' ? `/htree/${parsed.npub}` : `/${parsed.npub}`;
    return `${prefix}/${encodedTreeName}${encodedPath ? `/${encodedPath}` : ''}`;
  }

  const encodedPath = encodePath(parsed.path);
  return `/htree/${parsed.nhash}${encodedPath ? `/${encodedPath}` : ''}`;
}

export function resolveHtreeRequestUrl(
  input: string | ParsedHtreeUrl,
  options: ResolveHtreeRequestUrlOptions = {},
): string {
  const trimmedInput = typeof input === 'string' ? input.trim() : '';
  if (trimmedInput && /^https?:\/\//i.test(trimmedInput)) {
    return trimmedInput;
  }

  const baseUrl = options.baseUrl?.trim()
    ? options.baseUrl.trim().replace(/\/$/, '')
    : resolveRuntimeHtreeBaseUrl({
        windowLike: options.windowLike,
        fallbackBaseUrl: options.fallbackBaseUrl,
      });
  const mutableStyle = resolveMutableRequestStyle(options.windowLike, baseUrl, options.mutableStyle);
  const requestPath = buildHtreeRequestPath(input, mutableStyle);

  if (!requestPath) {
    return trimmedInput;
  }
  if (!baseUrl) {
    return requestPath;
  }
  return `${baseUrl}${requestPath}`;
}
