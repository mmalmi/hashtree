import { encryptionMode } from '@hashtree/core';
/**
 * Uploads must always point to encrypted content CIDs.
 */
export function assertEncryptedUploadCid(cid) {
    if (!cid.key) {
        throw new Error('Refusing to upload unencrypted CID');
    }
    try {
        encryptionMode(cid.key);
    }
    catch {
        throw new Error('Refusing to upload CID with invalid encryption key');
    }
}
/**
 * Mark known-encrypted block hashes as safe for peer serving.
 */
export function markEncryptedHashes(hashes, allowlist) {
    for (const hashHex of hashes) {
        allowlist.add(hashHex.toLowerCase());
    }
}
/**
 * Peer responses are restricted to hashes explicitly marked as encrypted.
 */
export function shouldServeHashToPeer(hashHex, allowlist) {
    return allowlist.has(hashHex.toLowerCase());
}
//# sourceMappingURL=privacyGuards.js.map