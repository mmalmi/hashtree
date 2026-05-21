import type { CID } from '@hashtree/core';
/**
 * Uploads must always point to encrypted content CIDs.
 */
export declare function assertEncryptedUploadCid(cid: CID): void;
/**
 * Mark known-encrypted block hashes as safe for peer serving.
 */
export declare function markEncryptedHashes(hashes: Iterable<string>, allowlist: Set<string>): void;
/**
 * Peer responses are restricted to hashes explicitly marked as encrypted.
 */
export declare function shouldServeHashToPeer(hashHex: string, allowlist: ReadonlySet<string>): boolean;
//# sourceMappingURL=privacyGuards.d.ts.map