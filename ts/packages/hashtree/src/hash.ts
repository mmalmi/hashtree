/**
 * Hashing utilities using Web Crypto API (browser-only)
 */

import { Hash } from './types.js';

/**
 * Compute SHA256 hash of data
 * Uses Web Crypto API - browser environments only
 */
export async function sha256(data: Uint8Array): Promise<Hash> {
  const buffer = new ArrayBuffer(data.length);
  new Uint8Array(buffer).set(data);
  const hashBuffer = await crypto.subtle.digest('SHA-256', buffer);
  return new Uint8Array(hashBuffer);
}

/**
 * Verify that data matches expected hash
 */
export async function verify(hash: Hash, data: Uint8Array): Promise<boolean> {
  const computed = await sha256(data);
  if (computed.length !== hash.length) return false;
  for (let i = 0; i < computed.length; i++) {
    if (computed[i] !== hash[i]) return false;
  }
  return true;
}
