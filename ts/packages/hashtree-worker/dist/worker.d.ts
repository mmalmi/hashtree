import type { WorkerResponse } from './protocol.js';
export interface HashtreeWorkerMessageEndpoint {
    postMessage(message: WorkerResponse): void;
    addEventListener(type: 'message', listener: EventListenerOrEventListenerObject): void;
    removeEventListener(type: 'message', listener: EventListenerOrEventListenerObject): void;
    start?: () => void;
}
export declare function attachHashtreeWorker(target?: HashtreeWorkerMessageEndpoint): () => void;
//# sourceMappingURL=worker.d.ts.map