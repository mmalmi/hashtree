import { fromHex, toHex } from '@hashtree/core';
export function serializeCid(cid) {
    if (!cid?.hash) {
        return null;
    }
    return {
        hash: toHex(cid.hash),
        key: cid.key ? toHex(cid.key) : undefined,
    };
}
export function deserializeCid(cid) {
    if (!cid?.hash) {
        return null;
    }
    return {
        hash: fromHex(cid.hash),
        key: cid.key ? fromHex(cid.key) : undefined,
    };
}
//# sourceMappingURL=cid.js.map