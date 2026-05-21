import type { WorkerConfig, BlossomServerConfig } from './protocol.js';
import type { HtreeRuntimeWindowLike } from './runtime.js';
import type { ParsedHtreeUrl, ResolveHtreeRequestUrlOptions } from './htree-url.js';
import type { HtreeClientIdStorageLike } from './client-id.js';
import { type RuntimeEndpoints } from './runtime-network.js';
export type RuntimeValueSource<T> = T | (() => T);
export interface HtreeRuntimeEndpointOverrides {
    relays?: readonly string[];
    blossomServers?: readonly BlossomServerConfig[];
}
export interface HtreeRuntimeOptions {
    appId?: string | null;
    fallbackBaseUrl?: string | null;
    windowLike?: HtreeRuntimeWindowLike;
    storage?: HtreeClientIdStorageLike | null;
    clientIdFactory?: () => string;
    clientIdStorageKey?: string;
    clientIdPrefix?: string;
    serviceWorker?: ServiceWorkerContainer | null;
    relays?: RuntimeValueSource<readonly string[]>;
    blossomServers?: RuntimeValueSource<readonly BlossomServerConfig[]>;
}
export interface HtreeRuntimeRequestUrlOptions extends Omit<ResolveHtreeRequestUrlOptions, 'windowLike' | 'fallbackBaseUrl'> {
}
export interface HtreeRuntimeMediaUrlOptions extends HtreeRuntimeRequestUrlOptions {
    clientScoped?: boolean;
    mimeType?: string | null | undefined;
    query?: Record<string, string | number | boolean | null | undefined>;
}
export interface HtreeRuntimeWorkerConfigOptions extends Omit<WorkerConfig, 'relays' | 'blossomServers'> {
    relays?: readonly string[];
    blossomServers?: readonly BlossomServerConfig[];
}
export interface HtreeRuntimeMediaPortOptions {
    registerMediaPort: (port: MessagePort, debug?: boolean) => Promise<void> | void;
    debug?: boolean;
    attempts?: number;
    delayMs?: number;
    pingTimeoutMs?: number;
    registrationTimeoutMs?: number;
    controllerTimeoutMs?: number;
}
export interface HtreeRuntime {
    readonly appId: string | null;
    readonly clientId: string | null;
    readonly endpoints: RuntimeEndpoints;
    getEndpoints(overrides?: HtreeRuntimeEndpointOverrides): RuntimeEndpoints;
    getWorkerConfig(options?: HtreeRuntimeWorkerConfigOptions): WorkerConfig;
    urls: {
        request: (input: string | ParsedHtreeUrl, options?: HtreeRuntimeRequestUrlOptions) => string;
        media: (input: string | ParsedHtreeUrl, options?: HtreeRuntimeMediaUrlOptions) => string;
        appendClientId: (url: string) => string;
    };
    media: {
        ensureReady: (options: HtreeRuntimeMediaPortOptions) => Promise<boolean>;
        reset: () => void;
    };
}
export declare function createHtreeRuntime(options?: HtreeRuntimeOptions): HtreeRuntime;
//# sourceMappingURL=app-runtime.d.ts.map