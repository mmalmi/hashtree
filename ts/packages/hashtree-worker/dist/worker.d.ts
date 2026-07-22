import { HashTree, type Store } from '@hashtree/core';
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
    handleExtensionRequest?: (request: unknown, runtime: HashtreeWorkerRuntime) => boolean;
}
export declare function attachHashtreeWorker(target?: HashtreeWorkerMessageEndpoint, options?: AttachHashtreeWorkerOptions): () => void;
//# sourceMappingURL=worker.d.ts.map