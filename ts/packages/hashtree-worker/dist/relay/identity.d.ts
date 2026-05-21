/**
 * Worker Identity Management
 *
 * Manages user identity for signing operations.
 * - For nsec login: secret key available, sign directly
 * - For extension login: delegate to main thread via NIP-07
 */
/**
 * Initialize identity from config
 */
export declare function initIdentity(pubkey: string, nsecHex?: string): void;
/**
 * Update identity (account switch)
 */
export declare function setIdentity(pubkey: string, nsecHex?: string): void;
/**
 * Clear identity on close
 */
export declare function clearIdentity(): void;
/**
 * Get user's pubkey (or ephemeral fallback)
 */
export declare function getPubkey(): string | null;
/**
 * Get user's secret key (null for extension login)
 */
export declare function getSecretKey(): Uint8Array | null;
/**
 * Get ephemeral secret key (fallback for sync signing)
 */
export declare function getEphemeralSecretKey(): Uint8Array | null;
/**
 * Check if we have a secret key for direct signing
 */
export declare function hasSecretKey(): boolean;
//# sourceMappingURL=identity.d.ts.map