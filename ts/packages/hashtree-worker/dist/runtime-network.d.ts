import type { BlossomServerConfig } from './protocol.js';
import { type HtreeRuntimeWindowLike } from './runtime.js';
export interface RuntimeNetworkOptions {
    windowLike?: HtreeRuntimeWindowLike;
}
export interface ResolveRuntimeEndpointsOptions extends RuntimeNetworkOptions {
    relays?: readonly string[];
    blossomServers?: readonly BlossomServerConfig[];
}
export interface RuntimeEndpoints {
    htreeServerUrl: string | null;
    nostrRelays: string[];
    blossomServers: BlossomServerConfig[];
}
export declare function normalizeRuntimeServerUrl(url: string): string;
export declare function normalizeRuntimeRelayUrl(url: string): string;
export declare function getRuntimeHtreeServerUrl(windowLike?: HtreeRuntimeWindowLike): string | null;
export declare function getRuntimeNostrRelayUrl(windowLike?: HtreeRuntimeWindowLike): string | null;
export declare function getRuntimeBlossomServer(windowLike?: HtreeRuntimeWindowLike): BlossomServerConfig | null;
export declare function getRuntimeNostrRelays(relays: readonly string[], options?: RuntimeNetworkOptions): string[];
export declare function getRuntimeBlossomServers(servers: readonly BlossomServerConfig[], options?: RuntimeNetworkOptions): BlossomServerConfig[];
export declare function resolveRuntimeEndpoints(options?: ResolveRuntimeEndpointsOptions): RuntimeEndpoints;
//# sourceMappingURL=runtime-network.d.ts.map