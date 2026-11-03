import { decryptObject, tryDecodeTreeNode } from '@hashtree/core';
/** A raw leaf carries its own plaintext bytes. A file node carries its children's sizes. */
export async function getEncryptedFileSize(tree, hash, key) {
    const stored = await tree.getBlob(hash);
    if (!stored)
        return null;
    const plaintext = await decryptObject(stored, key);
    const node = tryDecodeTreeNode(plaintext);
    if (!node)
        return plaintext.byteLength;
    const size = node.links.reduce((total, link) => total + (link.size ?? 0), 0);
    if (size > 0 || node.links.length === 0)
        return size;
    return (await tree.readFile({ hash, key }))?.byteLength ?? null;
}
//# sourceMappingURL=encryptedFileSize.js.map