import type { WorkerP2PProvider } from '../client.js';
import type { WebRTCController } from './webrtcController.js';
type MaybeController = WebRTCController | null | undefined;
export interface WebRTCWorkerP2PProviderOptions {
    getController: () => MaybeController;
    ensureController?: () => Promise<MaybeController> | MaybeController;
    canFetch?: () => boolean | Promise<boolean>;
    startupPeerWaitMs?: number;
    peerPollIntervalMs?: number;
}
export declare function createWebRTCWorkerP2PProvider(options: WebRTCWorkerP2PProviderOptions): WorkerP2PProvider;
export {};
//# sourceMappingURL=clientBridge.d.ts.map