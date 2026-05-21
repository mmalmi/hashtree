import { fromHex } from '@hashtree/core';
const DEFAULT_STARTUP_PEER_WAIT_MS = 5_000;
const DEFAULT_PEER_POLL_INTERVAL_MS = 100;
async function resolveController(options) {
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
function readConnectedPeerIds(options, fallback) {
    const controller = options.getController() ?? fallback;
    return controller.getConnectedHashGetPeerIds();
}
function readStartupPeerWaitMs(options) {
    return Math.max(0, options.startupPeerWaitMs ?? DEFAULT_STARTUP_PEER_WAIT_MS);
}
async function waitForController(options, deadline) {
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
async function waitForConnectedPeers(options, fallback, deadline) {
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
export function createWebRTCWorkerP2PProvider(options) {
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
            const hash = fromHex(hashHex);
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
//# sourceMappingURL=clientBridge.js.map