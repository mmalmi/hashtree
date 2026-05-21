/**
 * Creates a new instance of the secp256k1 WASM and returns the Nostr wrapper
 * @param z_src - a Response containing the WASM binary, a Promise that resolves to one,
 * 	or the raw bytes to the WASM binary as a {@link BufferSource}
 * @returns the wrapper API
 */
declare const NostrWasm: (z_src: any) => Promise<{
    generateSecretKey: () => Uint8Array<ArrayBuffer>;
    getPublicKey(sk: any): any;
    finalizeEvent(event: any, seckey: any, ent: any): void;
    verifyEvent(event: any): void;
}>;
export { NostrWasm };
//# sourceMappingURL=nostr-wasm.d.ts.map