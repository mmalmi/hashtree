import { type HashTree } from '@hashtree/core';
/** A raw leaf carries its own plaintext bytes. A file node carries its children's sizes. */
export declare function getEncryptedFileSize(tree: HashTree, hash: Uint8Array, key: Uint8Array): Promise<number | null>;
//# sourceMappingURL=encryptedFileSize.d.ts.map