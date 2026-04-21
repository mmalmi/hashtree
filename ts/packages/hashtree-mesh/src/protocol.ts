/**
 * Shared mesh data protocol utilities.
 */
import { encode, decode } from '@msgpack/msgpack';
import { sha256 } from '@hashtree/core';
import type { DataRequest, DataResponse, DataMessage } from './types.js';
import {
  BLOB_REQUEST_POLICY,
  HtlMode,
  type HtlPolicy,
  MSG_TYPE_REQUEST,
  MSG_TYPE_RESPONSE,
} from './types.js';

export function encodeRequest(req: DataRequest): ArrayBuffer {
  const body = encode(req);
  const result = new Uint8Array(1 + body.length);
  result[0] = MSG_TYPE_REQUEST;
  result.set(body, 1);
  return result.buffer;
}

export function encodeResponse(res: DataResponse): ArrayBuffer {
  const body = encode(res);
  const result = new Uint8Array(1 + body.length);
  result[0] = MSG_TYPE_RESPONSE;
  result.set(body, 1);
  return result.buffer;
}

export function parseMessage(data: ArrayBuffer | Uint8Array): DataMessage | null {
  try {
    const bytes = data instanceof ArrayBuffer ? new Uint8Array(data) : data;
    if (bytes.length < 2) return null;

    const type = bytes[0];
    const body = bytes.slice(1);

    if (type === MSG_TYPE_REQUEST) {
      return { type: MSG_TYPE_REQUEST, body: decode(body) as DataRequest };
    }
    if (type === MSG_TYPE_RESPONSE) {
      return { type: MSG_TYPE_RESPONSE, body: decode(body) as DataResponse };
    }
    return null;
  } catch {
    return null;
  }
}

export async function verifyHash(data: Uint8Array, expectedHash: Uint8Array): Promise<boolean> {
  const computedHash = await sha256(data);
  if (computedHash.length !== expectedHash.length) return false;
  for (let i = 0; i < computedHash.length; i += 1) {
    if (computedHash[i] !== expectedHash[i]) return false;
  }
  return true;
}

export interface PendingRequest {
  hash: Uint8Array;
  resolve: (data: Uint8Array | null) => void;
  timeout: ReturnType<typeof setTimeout>;
  startedAt?: number;
}

export interface PeerHTLConfig {
  atMaxSample: number;
  atMinSample: number;
  decrementAtMax?: boolean;
  decrementAtMin?: boolean;
}

export function generatePeerHTLConfig(): PeerHTLConfig {
  return {
    atMaxSample: Math.random(),
    atMinSample: Math.random(),
  };
}

export function decrementHTLWithPolicy(
  htl: number,
  policy: HtlPolicy,
  config: PeerHTLConfig,
): number {
  if (htl <= 0) return 0;
  const bounded = Math.min(htl, policy.maxHtl);
  const maxSample = config.atMaxSample ?? (config.decrementAtMax ? 0 : 1);
  const minSample = config.atMinSample ?? (config.decrementAtMin ? 0 : 1);

  if (policy.mode !== HtlMode.Probabilistic) {
    return Math.max(0, bounded - 1);
  }

  const pAtMax = Math.max(0, Math.min(1, policy.pAtMax));
  const pAtMin = Math.max(0, Math.min(1, policy.pAtMin));

  if (bounded === policy.maxHtl) {
    return maxSample < pAtMax ? bounded - 1 : bounded;
  }

  if (bounded === 1) {
    return minSample < pAtMin ? 0 : 1;
  }

  return bounded - 1;
}

export function decrementHTL(htl: number, config: PeerHTLConfig): number {
  return decrementHTLWithPolicy(htl, BLOB_REQUEST_POLICY, config);
}

export function shouldForwardHTL(htl: number): boolean {
  return htl > 0;
}

export function shouldForward(htl: number): boolean {
  return shouldForwardHTL(htl);
}

export function createRequest(hash: Uint8Array, htl: number = BLOB_REQUEST_POLICY.maxHtl): DataRequest {
  return { h: hash, htl };
}

export function createResponse(hash: Uint8Array, data: Uint8Array): DataResponse {
  return { h: hash, d: data };
}

export function createFragmentResponse(
  hash: Uint8Array,
  data: Uint8Array,
  index: number,
  total: number,
): DataResponse {
  return { h: hash, d: data, i: index, n: total };
}

export function isFragmented(res: DataResponse): boolean {
  return res.i !== undefined && res.n !== undefined;
}

export function hashToKey(hash: Uint8Array): string {
  let key = '';
  for (let i = 0; i < hash.length; i += 1) {
    key += hash[i].toString(16).padStart(2, '0');
  }
  return key;
}

export async function handleResponse(
  res: DataResponse,
  pendingRequests: Map<string, PendingRequest>,
): Promise<boolean | undefined> {
  const key = hashToKey(res.h);
  const pending = pendingRequests.get(key);
  if (!pending) return undefined;

  clearTimeout(pending.timeout);
  pendingRequests.delete(key);

  const isValid = await verifyHash(res.d, res.h);
  if (isValid) {
    pending.resolve(res.d);
  } else {
    pending.resolve(null);
  }
  return isValid;
}

export function clearPendingRequests(pendingRequests: Map<string, PendingRequest>): void {
  for (const pending of pendingRequests.values()) {
    clearTimeout(pending.timeout);
    pending.resolve(null);
  }
  pendingRequests.clear();
}
