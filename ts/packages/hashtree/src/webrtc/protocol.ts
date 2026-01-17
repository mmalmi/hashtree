/**
 * Shared protocol utilities for hashtree data exchange
 *
 * Used by WebRTC peer.ts implementation.
 *
 * Wire format: [type byte][msgpack body]
 * Request:  [0x00][msgpack: {id: u32, h: bytes32, htl?: u8}]
 * Response: [0x01][msgpack: {id: u32, h: bytes32, d: bytes}]
 */
import { encode, decode } from '@msgpack/msgpack';
import { sha256 } from '../hash.js';
import type { DataRequest, DataResponse, DataMessage } from './types.js';
import {
  MAX_HTL,
  DECREMENT_AT_MAX_PROB,
  DECREMENT_AT_MIN_PROB,
  MSG_TYPE_REQUEST,
  MSG_TYPE_RESPONSE,
} from './types.js';

/**
 * Encode a request message to wire format
 */
export function encodeRequest(req: DataRequest): ArrayBuffer {
  const body = encode(req);
  const result = new Uint8Array(1 + body.length);
  result[0] = MSG_TYPE_REQUEST;
  result.set(body, 1);
  return result.buffer;
}

/**
 * Encode a response message to wire format
 */
export function encodeResponse(res: DataResponse): ArrayBuffer {
  const body = encode(res);
  const result = new Uint8Array(1 + body.length);
  result[0] = MSG_TYPE_RESPONSE;
  result.set(body, 1);
  return result.buffer;
}

/**
 * Parse a wire format message
 */
export function parseMessage(data: ArrayBuffer | Uint8Array): DataMessage | null {
  try {
    const bytes = data instanceof ArrayBuffer ? new Uint8Array(data) : data;
    if (bytes.length < 2) return null;

    const type = bytes[0];
    const body = bytes.slice(1);

    if (type === MSG_TYPE_REQUEST) {
      return { type: MSG_TYPE_REQUEST, body: decode(body) as DataRequest };
    } else if (type === MSG_TYPE_RESPONSE) {
      return { type: MSG_TYPE_RESPONSE, body: decode(body) as DataResponse };
    }
    return null;
  } catch {
    return null;
  }
}

/**
 * Verify that data matches its expected hash
 */
export async function verifyHash(data: Uint8Array, expectedHash: Uint8Array): Promise<boolean> {
  const computedHash = await sha256(data);
  if (computedHash.length !== expectedHash.length) return false;
  for (let i = 0; i < computedHash.length; i++) {
    if (computedHash[i] !== expectedHash[i]) return false;
  }
  return true;
}

/**
 * Pending request tracking
 */
export interface PendingRequest {
  hash: Uint8Array;
  resolve: (data: Uint8Array | null) => void;
  timeout: ReturnType<typeof setTimeout>;
}

/**
 * Per-peer HTL config (Freenet-style probabilistic decrement)
 * Generated once per peer connection, stays fixed for connection lifetime
 */
export interface PeerHTLConfig {
  decrementAtMax: boolean;  // true = decrement at MAX_HTL
  decrementAtMin: boolean;  // true = decrement at HTL=1
}

/**
 * Generate random HTL config for a new peer connection
 */
export function generatePeerHTLConfig(): PeerHTLConfig {
  return {
    decrementAtMax: Math.random() < DECREMENT_AT_MAX_PROB,
    decrementAtMin: Math.random() < DECREMENT_AT_MIN_PROB,
  };
}

/**
 * Decrement HTL using peer's config (Freenet-style probabilistic)
 * Called when SENDING to a peer, not on receive
 */
export function decrementHTL(htl: number, config: PeerHTLConfig): number {
  if (htl <= 0) return 0;

  if (htl === MAX_HTL) {
    // At max: only decrement if this peer's config says so
    return config.decrementAtMax ? htl - 1 : htl;
  } else if (htl === 1) {
    // At min: only decrement if this peer's config says so
    return config.decrementAtMin ? 0 : htl;
  } else {
    // Middle values: always decrement
    return htl - 1;
  }
}

/**
 * Check if a request should be forwarded based on HTL
 */
export function shouldForward(htl: number): boolean {
  return htl > 0;
}

/**
 * Create a request body
 */
export function createRequest(hash: Uint8Array, htl: number = MAX_HTL): DataRequest {
  return { h: hash, htl };
}

/**
 * Create a response body
 */
export function createResponse(hash: Uint8Array, data: Uint8Array): DataResponse {
  return { h: hash, d: data };
}

/**
 * Create a fragmented response body
 */
export function createFragmentResponse(
  hash: Uint8Array,
  data: Uint8Array,
  index: number,
  total: number
): DataResponse {
  return { h: hash, d: data, i: index, n: total };
}

/**
 * Check if a response is fragmented
 */
export function isFragmented(res: DataResponse): boolean {
  return res.i !== undefined && res.n !== undefined;
}

/**
 * Convert hash to hex string for use as map key
 */
export function hashToKey(hash: Uint8Array): string {
  let key = '';
  for (let i = 0; i < hash.length; i++) {
    key += hash[i].toString(16).padStart(2, '0');
  }
  return key;
}

/**
 * Handle a response message - verify hash and resolve pending request
 * Returns true if hash verified, false if hash mismatch, undefined if no pending request
 */
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

/**
 * Clear all pending requests (on disconnect/close)
 */
export function clearPendingRequests(pendingRequests: Map<string, PendingRequest>): void {
  for (const pending of pendingRequests.values()) {
    clearTimeout(pending.timeout);
    pending.resolve(null);
  }
  pendingRequests.clear();
}
