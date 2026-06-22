/**
 * MessagePack encoding/decoding for tree nodes
 *
 * Blobs are stored raw (not wrapped) for efficiency.
 * Tree nodes are MessagePack-encoded.
 *
 * **Determinism:** We ensure deterministic output by:
 * 1. Using fixed field order in the encoded map
 * 2. Sorting metadata keys alphabetically before encoding
 * 3. Sorting directory links by BUD-16 name order before encoding
 *
 * File-node link order is preserved because chunk order is semantic.
 */

import { encode, decode } from '@msgpack/msgpack';
import { TreeNode, Link, LinkType, Hash } from './types.js';
import { sha256 } from './hash.js';
import { compareNames } from './compare.js';

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
  /** type - 0=Blob, 1=File, 2=Dir, 3=Fanout */
  t: number;
  /** metadata (optional) - keys must be sorted for determinism */
  m?: Record<string, unknown>;
}

/**
 * Internal MessagePack representation of a tree node
 */
interface TreeNodeMsgpack {
  /** type - 1=File, 2=Dir, 3=Fanout */
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

function linksForEncoding(node: TreeNode): Link[] {
  if (node.type !== LinkType.Dir) return node.links;
  return [...node.links].sort((left, right) =>
    compareNames(left.name ?? '', right.name ?? '')
  );
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function isKnownNodeType(type: unknown): type is LinkType.File | LinkType.Dir | LinkType.Fanout {
  return type === LinkType.File || type === LinkType.Dir || type === LinkType.Fanout;
}

function isKnownLinkType(type: unknown): type is LinkType {
  return (
    type === LinkType.Blob ||
    type === LinkType.File ||
    type === LinkType.Dir ||
    type === LinkType.Fanout
  );
}

/**
 * Encode a tree node to MessagePack
 * Fields are ordered alphabetically for canonical encoding
 */
export function encodeTreeNode(node: TreeNode): Uint8Array {
  const links = linksForEncoding(node);
  // TreeNode fields in alphabetical order: l, t
  const msgpack: TreeNodeMsgpack = {
    l: links.map(link => {
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
 * Returns null for non-tree blobs and throws for unsupported tree-shaped data.
 */
export function tryDecodeTreeNode(data: Uint8Array): TreeNode | null {
  let msgpack: unknown;
  try {
    msgpack = decode(data) as TreeNodeMsgpack;
  } catch {
    return null;
  }

  if (!isRecord(msgpack)) return null;
  if (!('t' in msgpack) || !('l' in msgpack)) return null;
  const nodeType = msgpack.t;
  if (!isKnownNodeType(nodeType)) {
    throw new Error(`Invalid node type: ${String(nodeType)}`);
  }
  const links = msgpack.l;
  if (!Array.isArray(links)) {
    throw new Error('Invalid tree links');
  }

  const node: TreeNode = {
    type: nodeType,
    links: links.map(linkValue => {
      if (!isRecord(linkValue)) {
        throw new Error('Invalid link');
      }
      const linkType = linkValue.t ?? LinkType.Blob;
      if (!isKnownLinkType(linkType)) {
        throw new Error(`Invalid link type: ${String(linkType)}`);
      }
      const link: Link = {
        hash: linkValue.h as Uint8Array,
        size: (linkValue.s as number | undefined) ?? 0,
        type: linkType,
      };
      if (linkValue.n !== undefined) link.name = linkValue.n as string;
      if (linkValue.k !== undefined) link.key = linkValue.k as Uint8Array;
      if (linkValue.m !== undefined) link.meta = linkValue.m as Record<string, unknown>;
      return link;
    }),
  };

  return node;
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
