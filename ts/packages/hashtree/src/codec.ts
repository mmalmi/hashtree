/**
 * MessagePack encoding/decoding for tree nodes
 *
 * Blobs are stored raw (not wrapped) for efficiency.
 * Tree nodes are MessagePack-encoded.
 *
 * **Determinism:** We ensure deterministic output by:
 * 1. Using fixed field order in the encoded map
 * 2. Sorting metadata keys alphabetically before encoding
 */

import { encode, decode } from '@msgpack/msgpack';
import { TreeNode, Link, LinkType, Hash } from './types.js';
import { sha256 } from './hash.js';

/**
 * Internal MessagePack representation of a link
 * Using short keys for compact encoding
 */
interface LinkMsgpack {
  /** hash */
  h: Uint8Array;
  /** name (optional) */
  n?: string;
  /** size (required) */
  s: number;
  /** CHK decryption key (optional) */
  k?: Uint8Array;
  /** type - 0=Blob, 1=File, 2=Dir */
  t: number;
  /** metadata (optional) - keys must be sorted for determinism */
  m?: Record<string, unknown>;
}

/**
 * Internal MessagePack representation of a tree node
 */
interface TreeNodeMsgpack {
  /** type - 1=File, 2=Dir */
  t: number;
  /** links */
  l: LinkMsgpack[];
}

/**
 * Sort object keys alphabetically for deterministic encoding
 */
function sortObjectKeys<T extends Record<string, unknown>>(obj: T): T {
  const sorted: Record<string, unknown> = {};
  for (const key of Object.keys(obj).sort()) {
    sorted[key] = obj[key];
  }
  return sorted as T;
}

/**
 * Encode a tree node to MessagePack
 * Fields are ordered alphabetically for canonical encoding
 */
export function encodeTreeNode(node: TreeNode): Uint8Array {
  // TreeNode fields in alphabetical order: l, t
  const msgpack: TreeNodeMsgpack = {
    l: node.links.map(link => {
      // Link fields in alphabetical order: h, k?, m?, n?, s, t
      // Build object with all fields in order, undefined values are omitted by msgpack
      const l: LinkMsgpack = {
        h: link.hash,
        k: link.key,
        m: link.meta !== undefined ? sortObjectKeys(link.meta) : undefined,
        n: link.name,
        s: link.size,
        t: link.type,
      } as LinkMsgpack;
      // Remove undefined fields to match skip_serializing_if behavior
      if (l.k === undefined) delete l.k;
      if (l.m === undefined) delete l.m;
      if (l.n === undefined) delete l.n;
      return l;
    }),
    t: node.type,
  };

  return encode(msgpack);
}

/**
 * Try to decode MessagePack data as a tree node
 * Returns null if data is not a valid tree node (i.e., it's a raw blob)
 */
export function tryDecodeTreeNode(data: Uint8Array): TreeNode | null {
  try {
    const msgpack = decode(data) as TreeNodeMsgpack;

    if (msgpack.t !== LinkType.File && msgpack.t !== LinkType.Dir) {
      return null;
    }

    const node: TreeNode = {
      type: msgpack.t as LinkType.File | LinkType.Dir,
      links: msgpack.l.map(l => {
        const link: Link = { hash: l.h, size: l.s ?? 0, type: l.t ?? LinkType.Blob };
        if (l.n !== undefined) link.name = l.n;
        if (l.k !== undefined) link.key = l.k;
        if (l.m !== undefined) link.meta = l.m;
        return link;
      }),
    };

    return node;
  } catch {
    return null;
  }
}

/**
 * Decode MessagePack to a tree node (throws if not a tree node)
 */
export function decodeTreeNode(data: Uint8Array): TreeNode {
  const node = tryDecodeTreeNode(data);
  if (!node) {
    throw new Error('Data is not a valid tree node');
  }
  return node;
}

/**
 * Encode a tree node and compute its hash
 */
export async function encodeAndHash(node: TreeNode): Promise<{ data: Uint8Array; hash: Hash }> {
  const data = encodeTreeNode(node);
  const hash = await sha256(data);
  return { data, hash };
}

/**
 * Get the type of a chunk: File, Dir, or Blob
 */
export function getNodeType(data: Uint8Array): LinkType {
  const node = tryDecodeTreeNode(data);
  return node?.type ?? LinkType.Blob;
}

