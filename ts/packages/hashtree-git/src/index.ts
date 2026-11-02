import type { CID, TreeVisibility } from '@hashtree/core';
import { fromHex, toHex, objectKeyFromHex, decryptKeyFromLink } from '@hashtree/core';

const HEX_32_RE = /^[0-9a-fA-F]{64}$/;
const KEY_HEX = /^[0-9a-fA-F]{64}(?:[0-9a-fA-F]{2})?$/;



export interface HtreeUrlParts {
  identifier: string;
  repo: string;
  visibility: TreeVisibility;
  linkKey?: string;
  autoGenerateLinkKey: boolean;
}

export interface HtreeUrlOptions {
  visibility?: TreeVisibility;
  linkKey?: string;
  autoGenerateLinkKey?: boolean;
}

/**
 * Parse htree://<identifier>/<repo>[#fragment]
 *
 * Supported fragments:
 * - #k=<64-or-66-hex-chars> (link-visible with explicit key)
 * - #link-visible (auto-generate key on push)
 * - #private (author-only)
 */
export function parseHtreeUrl(uri: string): HtreeUrlParts {
  if (!uri.startsWith('htree://')) {
    throw new Error('Invalid Htree URI: must start with "htree://"');
  }

  const withoutScheme = uri.slice('htree://'.length);
  const [pathPart, fragment] = withoutScheme.split('#', 2);

  let visibility: TreeVisibility = 'public';
  let linkKey: string | undefined;
  let autoGenerateLinkKey = false;

  if (fragment) {
    if (fragment === 'private') {
      visibility = 'private';
    } else if (fragment === 'link-visible') {
      visibility = 'link-visible';
      autoGenerateLinkKey = true;
    } else if (fragment.startsWith('k=')) {
      const keyHex = fragment.slice(2);
      if (!KEY_HEX.test(keyHex)) {
        throw new Error('Invalid Htree URI: secret key must be 64 or 66 hex characters');
      }
      visibility = 'link-visible';
      linkKey = keyHex.toLowerCase();
    } else {
      throw new Error('Invalid Htree URI: unknown fragment');
    }
  }

  const [identifier, ...repoParts] = pathPart.split('/');
  const repo = repoParts.join('/');

  if (!identifier) {
    throw new Error('Invalid Htree URI: identifier is required');
  }
  if (!repo) {
    throw new Error('Invalid Htree URI: repository name is required');
  }

  return {
    identifier,
    repo,
    visibility,
    linkKey,
    autoGenerateLinkKey,
  };
}

/**
 * Build htree:// clone URL for the repository.
 */
export function buildHtreeUrl(identifier: string, repo: string, options: HtreeUrlOptions = {}): string {
  let url = `htree://${identifier}/${repo}`;

  if (options.visibility === 'private') {
    url += '#private';
  } else if (options.visibility === 'link-visible') {
    if (options.linkKey) {
      const keyHex = options.linkKey;
      if (!KEY_HEX.test(keyHex)) {
        throw new Error('Invalid Htree URI: secret key must be 64 or 66 hex characters');
      }
      url += `#k=${keyHex.toLowerCase()}`;
    } else if (options.autoGenerateLinkKey) {
      url += '#link-visible';
    }
  }

  return url;
}

export function formatHtreeUrl(parts: HtreeUrlParts): string {
  return buildHtreeUrl(parts.identifier, parts.repo, {
    visibility: parts.visibility,
    linkKey: parts.linkKey,
    autoGenerateLinkKey: parts.autoGenerateLinkKey,
  });
}

export interface HtreeVisibilityInfo {
  visibility: TreeVisibility;
  rootKey?: string;
  encryptedKey?: string;
  keyId?: string;
  selfEncryptedKey?: string;
  selfEncryptedLinkKey?: string;
}

/**
 * Parse visibility from Nostr event tags.
 */
export function parseHtreeVisibility(tags: string[][]): HtreeVisibilityInfo {
  const rootKey = tags.find((tag) => tag[0] === 'key')?.[1];
  const encryptedKey = tags.find((tag) => tag[0] === 'encryptedKey')?.[1];
  const keyId = tags.find((tag) => tag[0] === 'keyId')?.[1];
  const selfEncryptedKey = tags.find((tag) => tag[0] === 'selfEncryptedKey')?.[1];
  const selfEncryptedLinkKey = tags.find((tag) => tag[0] === 'selfEncryptedLinkKey')?.[1];

  let visibility: TreeVisibility;
  if (encryptedKey) {
    visibility = 'link-visible';
  } else if (selfEncryptedKey) {
    visibility = 'private';
  } else {
    visibility = 'public';
  }

  return { visibility, rootKey, encryptedKey, keyId, selfEncryptedKey, selfEncryptedLinkKey };
}

export interface ResolveHtreeRootCidOptions {
  tags: string[][];
  content?: string;
  linkKey?: Uint8Array;
  requirePrivate?: boolean;
  decryptSelfKey?: (payload: string) => Promise<string | Uint8Array>;
}

/**
 * Resolve the root CID from a hashtree event.
 */
export async function resolveHtreeRootCid(options: ResolveHtreeRootCidOptions): Promise<CID> {
  const { tags, content, linkKey, requirePrivate, decryptSelfKey } = options;

  const hashTag = tags.find((tag) => tag[0] === 'hash')?.[1];
  const rootHashHex = (hashTag ?? content ?? '').trim();

  if (!HEX_32_RE.test(rootHashHex)) {
    throw new Error('Invalid Htree root hash');
  }

  const rootHash = fromHex(rootHashHex.toLowerCase());
  const keyTag = tags.find((tag) => tag.length >= 2 && (tag[0] === 'key' || tag[0] === 'encryptedKey' || tag[0] === 'selfEncryptedKey'));

  if (!keyTag || !keyTag[1]) {
    return { hash: rootHash };
  }

  const keyType = keyTag[0];
  const keyValue = keyTag[1];

  if (keyType === 'key') {
    return { hash: rootHash, key: objectKeyFromHex(keyValue) };
  }

  if (keyType === 'encryptedKey') {
    if (!linkKey) {
      throw new Error('Htree repository requires a secret key');
    }
    if (!KEY_HEX.test(keyValue)) throw new Error('Invalid Htree link-visible key');
    const unmasked = await decryptKeyFromLink(fromHex(keyValue), linkKey);
    if (!unmasked) throw new Error('Invalid Htree link-visible key');
    return { hash: rootHash, key: unmasked };
  }

  if (keyType === 'selfEncryptedKey') {
    if (!requirePrivate) {
      throw new Error('Htree repository is private');
    }
    if (!decryptSelfKey) {
      throw new Error('Decrypt callback required for private Htree repositories');
    }
    const decrypted = await decryptSelfKey(keyValue);
    const decryptedHex = typeof decrypted === 'string' ? decrypted : toHex(decrypted);
    let keyBytes: Uint8Array;
    try {
      keyBytes = objectKeyFromHex(decryptedHex);
    } catch (cause) {
      throw new Error('Invalid decrypted Htree key', { cause });
    }
    return { hash: rootHash, key: keyBytes };
  }

  return { hash: rootHash };
}
