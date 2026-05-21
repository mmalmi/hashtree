// Worker replies may reuse bytes from caches or stores. Copy them before
// transfer so postMessage ownership changes do not detach the source buffer.
export function cloneTransferableBytes(bytes) {
    return bytes.slice();
}
//# sourceMappingURL=transferableBytes.js.map