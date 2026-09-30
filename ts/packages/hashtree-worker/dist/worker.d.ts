import { HashTree, type Store } from '@hashtree/core';
import { type RootNostrSubscribe } from './capabilities/rootResolver.js';
export interface HashtreeWorkerMessageEndpoint {
    postMessage(message: unknown, transfer?: Transferable[]): void;
    addEventListener(type: 'message', listener: EventListenerOrEventListenerObject): void;
    removeEventListener(type: 'message', listener: EventListenerOrEventListenerObject): void;
    start?: () => void;
}
export interface HashtreeWorkerRuntime {
    readonly tree: HashTree | null;
    readonly store: Store | null;
    postMessage(message: unknown, transfer?: Transferable[]): void;
}
export interface AttachHashtreeWorkerOptions {
    /** Reuse the app's worker-owned event runtime for mutable root reads/watches. */
    nostrSubscribe?: RootNostrSubscribe;
    handleExtensionRequest?: (request: unknown, runtime: HashtreeWorkerRuntime) => boolean;
}
export declare function attachHashtreeWorker(target?: HashtreeWorkerMessageEndpoint, options?: AttachHashtreeWorkerOptions): () => void;
//# sourceMappingURL=worker.d.ts.map