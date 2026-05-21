import { createWebRTCWorkerP2PProvider } from './clientBridge.js';
import { createSignalingFilters, decodeSignalingEvent } from './signaling.js';
import { SimplePool } from 'nostr-tools/pool';
import { WebRTCController } from './webrtcController.js';
import { WebRTCProxy } from './webrtcProxy.js';
const DEFAULT_HEALTH_CHECK_INTERVAL_MS = 2_000;
const DEFAULT_REANNOUNCE_INTERVAL_MS = 4_000;
const DEFAULT_RESTART_INTERVAL_MS = 8_000;
export class ManagedWebRTCMeshHost {
    healthCheckIntervalMs;
    reannounceIntervalMs;
    restartIntervalMs;
    createSignalPool;
    createController;
    createProxy;
    workerClient = null;
    desiredSession = null;
    active = null;
    syncVersion = 0;
    currentSync = null;
    uploadLimitBytesPerSecond = null;
    poolConfig = null;
    healthCheckTimer = null;
    lastHealthyAt = 0;
    lastHelloAt = 0;
    lastRestartAt = 0;
    constructor(options = {}) {
        this.healthCheckIntervalMs = options.healthCheckIntervalMs ?? DEFAULT_HEALTH_CHECK_INTERVAL_MS;
        this.reannounceIntervalMs = options.reannounceIntervalMs ?? DEFAULT_REANNOUNCE_INTERVAL_MS;
        this.restartIntervalMs = options.restartIntervalMs ?? DEFAULT_RESTART_INTERVAL_MS;
        this.createSignalPool = options.createSignalPool ?? (() => new SimplePool({ enableReconnect: true }));
        this.createController = options.createController ?? ((config) => new WebRTCController(config));
        this.createProxy = options.createProxy ?? ((onEvent, maxUploadBytesPerSecond) => (new WebRTCProxy(onEvent, { maxUploadBytesPerSecond })));
    }
    attachWorkerClient(client, options = {}) {
        if (this.workerClient && this.workerClient !== client) {
            this.workerClient.setP2PProvider(null);
        }
        this.workerClient = client;
        client.setP2PProvider(createWebRTCWorkerP2PProvider({
            getController: () => this.active?.controller ?? null,
            ensureController: async () => {
                await this.ensureStarted();
                return this.active?.controller ?? null;
            },
            canFetch: options.canFetch,
        }));
    }
    async setSession(session, force = false) {
        this.desiredSession = session;
        const version = ++this.syncVersion;
        const syncPromise = this.runSync(version, force);
        this.currentSync = syncPromise;
        await syncPromise;
    }
    setUploadLimitBytesPerSecond(maxUploadBytesPerSecond) {
        this.uploadLimitBytesPerSecond = maxUploadBytesPerSecond ?? null;
        this.active?.proxy.setUploadLimitBytesPerSecond(this.uploadLimitBytesPerSecond);
    }
    setPoolConfig(config) {
        this.poolConfig = config;
        this.active?.controller.setPoolConfig(config);
    }
    broadcastHello() {
        this.lastHelloAt = Date.now();
        this.active?.controller.broadcastHello();
    }
    getController() {
        return this.active?.controller ?? null;
    }
    getPubkey() {
        return this.active?.pubkey ?? null;
    }
    getActiveSignature() {
        return this.active?.signature ?? null;
    }
    getRelayUrls() {
        return this.active?.relayUrls.slice() ?? this.desiredSession?.relayUrls.slice() ?? [];
    }
    getRelayConnectionStatus() {
        return new Map(this.active?.signalPool.listConnectionStatus() ?? []);
    }
    getConnectedPeerCount() {
        return this.active?.controller.getConnectedCount() ?? 0;
    }
    getPeerStats() {
        return this.active?.controller.getPeerStats() ?? [];
    }
    isActive() {
        return this.active !== null;
    }
    async close() {
        this.desiredSession = null;
        ++this.syncVersion;
        if (this.currentSync) {
            await this.currentSync.catch(() => undefined);
        }
        await this.disposeActive();
        this.stopHealthCheck();
        if (this.workerClient) {
            this.workerClient.setP2PProvider(null);
            this.workerClient = null;
        }
    }
    async ensureStarted() {
        if (this.active || !this.desiredSession) {
            return;
        }
        if (this.currentSync) {
            await this.currentSync.catch(() => undefined);
            if (this.active || !this.desiredSession) {
                return;
            }
        }
        await this.setSession(this.desiredSession, false);
    }
    startHealthCheck() {
        if (this.healthCheckTimer || this.healthCheckIntervalMs <= 0) {
            return;
        }
        this.healthCheckTimer = setInterval(() => {
            this.checkHealth();
        }, this.healthCheckIntervalMs);
    }
    stopHealthCheck() {
        if (!this.healthCheckTimer) {
            return;
        }
        clearInterval(this.healthCheckTimer);
        this.healthCheckTimer = null;
    }
    checkHealth() {
        const active = this.active;
        const desiredSession = this.desiredSession;
        if (!active || !desiredSession) {
            return;
        }
        const relayConnected = Array.from(active.signalPool.listConnectionStatus().values()).some(Boolean);
        const connectedPeers = active.controller.getConnectedCount();
        const now = Date.now();
        if (relayConnected && connectedPeers > 0) {
            this.lastHealthyAt = now;
            return;
        }
        if (relayConnected && connectedPeers === 0 && now - this.lastHelloAt >= this.reannounceIntervalMs) {
            this.lastHelloAt = now;
            active.controller.broadcastHello();
        }
        const stalled = this.lastHealthyAt > 0 && now - this.lastHealthyAt >= this.restartIntervalMs;
        if ((!relayConnected || stalled) && now - this.lastRestartAt >= this.restartIntervalMs) {
            this.lastRestartAt = now;
            void this.setSession(desiredSession, true);
        }
    }
    async runSync(version, force) {
        const session = this.desiredSession;
        if (!session) {
            await this.disposeActive();
            this.stopHealthCheck();
            return;
        }
        this.startHealthCheck();
        if (!force && this.active?.signature === session.signature) {
            return;
        }
        if (force && this.active?.signature === session.signature) {
            this.lastRestartAt = Date.now();
        }
        await this.disposeActive();
        if (version !== this.syncVersion) {
            return;
        }
        let controller = null;
        const signalPool = this.createSignalPool();
        signalPool.onRelayConnectionSuccess = () => {
            const connectedRelays = Array.from(signalPool.listConnectionStatus().values()).filter(Boolean).length;
            if (connectedRelays === 1) {
                controller?.broadcastHello();
            }
        };
        const proxy = this.createProxy((event) => {
            controller?.handleProxyEvent(event);
        }, this.uploadLimitBytesPerSecond);
        const sendSignaling = session.sendSignaling
            ?? session.createSendSignaling?.({
                signalPool,
                relayUrls: session.relayUrls.slice(),
            });
        if (!sendSignaling) {
            proxy.close();
            signalPool.destroy();
            await session.closeLocalStore?.();
            throw new Error('ManagedWebRTCMeshSessionConfig requires sendSignaling or createSendSignaling');
        }
        controller = this.createController({
            pubkey: session.pubkey,
            localStore: session.localStore,
            sendCommand: (cmd) => {
                proxy.handleCommand(cmd);
            },
            sendSignaling,
            getFollows: session.getFollows,
            requestTimeout: session.requestTimeoutMs,
            forwardRateLimit: session.forwardRateLimit,
            upstreamFetch: session.upstreamFetch,
            debug: session.debug ?? false,
        });
        await controller.loadPeerMetadata().catch(() => false);
        const handleSignalEvent = (event) => {
            if (event.pubkey === session.pubkey) {
                return;
            }
            void this.routeSignalEvent(session, controller, event);
        };
        const { helloFilter, directedFilter } = createSignalingFilters(session.pubkey);
        const subscriptions = [
            signalPool.subscribe(session.relayUrls, helloFilter, { onevent: handleSignalEvent }),
            signalPool.subscribe(session.relayUrls, directedFilter, { onevent: handleSignalEvent }),
        ];
        if (this.poolConfig) {
            controller.setPoolConfig(this.poolConfig);
        }
        controller.start();
        if (version !== this.syncVersion) {
            controller.stop();
            proxy.close();
            for (const subscription of subscriptions) {
                subscription.close?.();
            }
            signalPool.destroy();
            await session.closeLocalStore?.();
            return;
        }
        this.active = {
            signature: session.signature,
            pubkey: session.pubkey,
            relayUrls: session.relayUrls.slice(),
            controller,
            proxy,
            signalPool,
            subscriptions,
            closeLocalStore: session.closeLocalStore,
        };
        this.lastHealthyAt = 0;
        this.lastHelloAt = 0;
    }
    async routeSignalEvent(session, controller, event) {
        const decoded = await decodeSignalingEvent({
            event,
            giftUnwrap: session.unwrapGift,
        });
        if (!decoded || decoded.senderPubkey === session.pubkey) {
            return;
        }
        await controller.handleSignalingMessage(decoded.message, decoded.senderPubkey);
    }
    async disposeActive() {
        const active = this.active;
        if (!active) {
            return;
        }
        this.active = null;
        await active.controller.persistPeerMetadata().catch(() => null);
        active.controller.stop();
        active.proxy.close();
        for (const subscription of active.subscriptions) {
            subscription.close?.();
        }
        active.signalPool.destroy();
        await active.closeLocalStore?.();
    }
}
//# sourceMappingURL=managedMeshHost.js.map