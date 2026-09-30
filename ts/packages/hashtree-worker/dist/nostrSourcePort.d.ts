import type { RuntimeSource } from 'nostr-pubsub';
/** Serve an existing source; this bridge never creates a relay pool or a FIPS node. */
export declare function serveNostrSource(port: MessagePort, source: RuntimeSource): () => void;
export declare function connectNostrSource(port: MessagePort, id: string, publishAcceptance?: RuntimeSource['publishAcceptance']): {
    source: RuntimeSource;
    close(): void;
};
//# sourceMappingURL=nostrSourcePort.d.ts.map