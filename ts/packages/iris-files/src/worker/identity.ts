/**
 * Worker Identity Management
 *
 * Manages user identity for signing operations.
 * - For nsec login: secret key available, sign directly
 * - For extension login: delegate to main thread via NIP-07
 */

import { generateSecretKey, getPublicKey } from 'nostr-tools';

// User identity (set via init or setIdentity)
let userPubkey: string | null = null;
let userSecretKey: Uint8Array | null = null;

// Fallback ephemeral identity (used before user logs in)
let ephemeralSecretKey: Uint8Array | null = null;
let ephemeralPubkey: string | null = null;

/**
 * Initialize identity from config
 */
export function initIdentity(pubkey: string, nsecHex?: string): void {
  userPubkey = pubkey;
  if (nsecHex) {
    userSecretKey = new Uint8Array(nsecHex.match(/.{2}/g)!.map(b => parseInt(b, 16)));
  } else {
    userSecretKey = null;
  }

  // Generate fallback ephemeral identity
  if (!ephemeralSecretKey) {
    ephemeralSecretKey = generateSecretKey();
    ephemeralPubkey = getPublicKey(ephemeralSecretKey);
  }
}

/**
 * Update identity (account switch)
 */
export function setIdentity(pubkey: string, nsecHex?: string): void {
  userPubkey = pubkey;
  if (nsecHex) {
    userSecretKey = new Uint8Array(nsecHex.match(/.{2}/g)!.map(b => parseInt(b, 16)));
  } else {
    userSecretKey = null;
  }
}

/**
 * Clear identity on close
 */
export function clearIdentity(): void {
  userPubkey = null;
  userSecretKey = null;
  ephemeralSecretKey = null;
  ephemeralPubkey = null;
}

/**
 * Get user's pubkey (or ephemeral fallback)
 */
export function getPubkey(): string | null {
  return userPubkey || ephemeralPubkey;
}

/**
 * Get user's secret key (null for extension login)
 */
export function getSecretKey(): Uint8Array | null {
  return userSecretKey;
}

/**
 * Get ephemeral secret key (fallback for sync signing)
 */
export function getEphemeralSecretKey(): Uint8Array | null {
  return ephemeralSecretKey;
}

/**
 * Check if we have a secret key for direct signing
 */
export function hasSecretKey(): boolean {
  return userSecretKey !== null;
}
