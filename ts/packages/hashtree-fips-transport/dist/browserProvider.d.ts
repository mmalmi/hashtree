import { type Store } from '@hashtree/core';
import { FipsNode, type FipsIdentity, type Logger } from '@fips/core';
import { WebRtcTransport } from '@fips/transport-webrtc';
import { type HashtreeWorkerP2PProvider } from './workerProvider.js';
interface BrowserHashtreeFipsProviderBaseOptions {
    relays: readonly string[];
    localStore: Store;
    discoveryApp?: string;
    stunServers?: readonly string[];
    maxConnections?: number;
    connectTimeoutMs?: number;
    relayConnectTimeoutMs?: number;
    iceGatherTimeoutMs?: number;
    requestTimeoutMs?: number;
    forwarding?: boolean;
    logger?: Logger;
}
export type BrowserHashtreeFipsProviderOptions = BrowserHashtreeFipsProviderBaseOptions & ({
    identity: FipsIdentity;
    deviceSecretKey?: never;
} | {
    identity?: never;
    deviceSecretKey: Uint8Array | string;
});
export interface BrowserHashtreeFipsProvider extends HashtreeWorkerP2PProvider {
    readonly node: FipsNode;
    readonly webRtcTransport: WebRtcTransport;
    stop(): Promise<void>;
}
export declare function supportsBrowserHashtreeFips(): boolean;
/**
 * Starts a browser FIPS node whose links and routes are discovered over Nostr,
 * then exposes Hashtree's worker-provider surface over authenticated FIPS data.
 */
export declare function createBrowserHashtreeFipsProvider(options: BrowserHashtreeFipsProviderOptions): Promise<BrowserHashtreeFipsProvider>;
export {};
//# sourceMappingURL=browserProvider.d.ts.map