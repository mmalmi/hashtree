import { fromHex, type Hash } from '@hashtree/core';
import type { WorkerP2PProvider } from '../client.js';
import type { WebRTCController } from './webrtcController.js';

type MaybeController = WebRTCController | null | undefined;
const DEFAULT_STARTUP_PEER_WAIT_MS = 5_000;
const DEFAULT_PEER_POLL_INTERVAL_MS = 100;

export interface WebRTCWorkerP2PProviderOptions {
  getController: () => MaybeController;
  ensureController?: () => Promise<MaybeController> | MaybeController;
  canFetch?: () => boolean | Promise<boolean>;
  startupPeerWaitMs?: number;
  peerPollIntervalMs?: number;
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

function readConnectedPeerIds(
  options: WebRTCWorkerP2PProviderOptions,
  fallback: WebRTCController,
): string[] {
  const controller = options.getController() ?? fallback;
  return controller.getConnectedHashGetPeerIds();
}

function readStartupPeerWaitMs(options: WebRTCWorkerP2PProviderOptions): number {
  return Math.max(0, options.startupPeerWaitMs ?? DEFAULT_STARTUP_PEER_WAIT_MS);
}

async function waitForController(
  options: WebRTCWorkerP2PProviderOptions,
  deadline: number,
): Promise<WebRTCController | null> {
  const pollIntervalMs = Math.max(1, options.peerPollIntervalMs ?? DEFAULT_PEER_POLL_INTERVAL_MS);
  let controller = await resolveController(options);
  if (controller || readStartupPeerWaitMs(options) === 0) {
    return controller;
  }

  while (!controller && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, Math.min(pollIntervalMs, deadline - Date.now())));
    controller = await resolveController(options);
  }

  return controller;
}

async function waitForConnectedPeers(
  options: WebRTCWorkerP2PProviderOptions,
  fallback: WebRTCController,
  deadline: number,
): Promise<WebRTCController> {
  if (readStartupPeerWaitMs(options) === 0 || readConnectedPeerIds(options, fallback).length > 0) {
    return options.getController() ?? fallback;
  }

  const pollIntervalMs = Math.max(1, options.peerPollIntervalMs ?? DEFAULT_PEER_POLL_INTERVAL_MS);
  while (Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, Math.min(pollIntervalMs, deadline - Date.now())));
    if (readConnectedPeerIds(options, fallback).length > 0) {
      return options.getController() ?? fallback;
    }
  }

  return options.getController() ?? fallback;
}

export function createWebRTCWorkerP2PProvider(
  options: WebRTCWorkerP2PProviderOptions,
): WorkerP2PProvider {
  return {
    fetch: async (hashHex, peerId) => {
      if (options.canFetch && !(await options.canFetch())) {
        return null;
      }

      const startupDeadline = Date.now() + readStartupPeerWaitMs(options);
      const controller = await waitForController(options, startupDeadline);
      if (!controller) {
        return null;
      }

      const liveController = peerId
        ? controller
        : await waitForConnectedPeers(options, controller, startupDeadline);
      const hash = fromHex(hashHex) as Hash;
      return peerId
        ? liveController.getFromPeer(peerId, hash)
        : liveController.get(hash);
    },
    listPeerIds: async () => {
      // Listing peers is used inside worker-side mesh routing and metadata loads.
      // It must never recursively bootstrap the controller, otherwise startup can
      // deadlock while the controller is still building its first session.
      const controller = options.getController();
      return controller ? controller.getConnectedHashGetPeerIds() : [];
    },
  };
}
