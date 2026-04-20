import { fromHex, type Hash } from '@hashtree/core';
import type { WorkerP2PProvider } from '../client.js';
import type { WebRTCController } from './webrtcController.js';

type MaybeController = WebRTCController | null | undefined;

export interface WebRTCWorkerP2PProviderOptions {
  getController: () => MaybeController;
  ensureController?: () => Promise<MaybeController> | MaybeController;
  canFetch?: () => boolean | Promise<boolean>;
}

async function resolveController(options: WebRTCWorkerP2PProviderOptions): Promise<WebRTCController | null> {
  const existing = options.getController();
  if (existing) {
    return existing;
  }

  if (!options.ensureController) {
    return null;
  }

  const resolved = await options.ensureController();
  return resolved ?? options.getController() ?? null;
}

export function createWebRTCWorkerP2PProvider(
  options: WebRTCWorkerP2PProviderOptions,
): WorkerP2PProvider {
  return {
    fetch: async (hashHex, peerId) => {
      if (options.canFetch && !(await options.canFetch())) {
        return null;
      }

      const controller = await resolveController(options);
      if (!controller) {
        return null;
      }

      const hash = fromHex(hashHex) as Hash;
      return peerId
        ? controller.getFromPeer(peerId, hash)
        : controller.get(hash);
    },
    listPeerIds: async () => {
      const controller = await resolveController(options);
      return controller ? controller.getConnectedHashGetPeerIds() : [];
    },
  };
}
