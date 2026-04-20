import type { Store } from '@hashtree/core';
import type { HashtreeWorkerClient } from '../client.js';
import { createWebRTCWorkerP2PProvider } from './clientBridge.js';
import type { WebRTCEvent } from './protocol.js';
import type { SignalingMessage } from '@hashtree/nostr';
import { createSignalingFilters, decodeSignalingEvent, type GiftSeal, type SignalingEventLike } from './signaling.js';
import { SimplePool } from 'nostr-tools/pool';
import { WebRTCController, type WebRTCControllerConfig } from './webrtcController.js';
import { WebRTCProxy } from './webrtcProxy.js';

export type WebRTCMeshPoolConfig = {
  follows: { max: number; satisfied: number };
  other: { max: number; satisfied: number };
};

export interface ManagedWebRTCMeshSessionConfig {
  signature: string;
  pubkey: string;
  relayUrls: string[];
  localStore: Store;
  closeLocalStore?: () => void | Promise<void>;
  sendSignaling?: (msg: SignalingMessage, recipientPubkey?: string) => Promise<void>;
  createSendSignaling?: (context: {
    signalPool: SimplePool;
    relayUrls: string[];
  }) => (msg: SignalingMessage, recipientPubkey?: string) => Promise<void>;
  unwrapGift: (event: SignalingEventLike) => Promise<GiftSeal | null>;
  getFollows?: () => Set<string>;
  requestTimeoutMs?: number;
  forwardRateLimit?: WebRTCControllerConfig['forwardRateLimit'];
  upstreamFetch?: (hash: Uint8Array) => Promise<Uint8Array | null>;
  debug?: boolean;
}

export interface ManagedWebRTCMeshHostOptions {
  healthCheckIntervalMs?: number;
  reannounceIntervalMs?: number;
  restartIntervalMs?: number;
  createSignalPool?: () => SimplePool;
  createController?: (config: WebRTCControllerConfig) => WebRTCController;
  createProxy?: (
    onEvent: (event: WebRTCEvent) => void,
    maxUploadBytesPerSecond: number | null,
  ) => WebRTCProxy;
}

type ActiveMesh = {
  signature: string;
  pubkey: string;
  relayUrls: string[];
  controller: WebRTCController;
  proxy: WebRTCProxy;
  signalPool: SimplePool;
  subscriptions: Array<{ close?: () => void }>;
  closeLocalStore?: () => void | Promise<void>;
};

const DEFAULT_HEALTH_CHECK_INTERVAL_MS = 2_000;
const DEFAULT_REANNOUNCE_INTERVAL_MS = 4_000;
const DEFAULT_RESTART_INTERVAL_MS = 8_000;

export class ManagedWebRTCMeshHost {
  private readonly healthCheckIntervalMs: number;
  private readonly reannounceIntervalMs: number;
  private readonly restartIntervalMs: number;
  private readonly createSignalPool: () => SimplePool;
  private readonly createController: (config: WebRTCControllerConfig) => WebRTCController;
  private readonly createProxy: (
    onEvent: (event: WebRTCEvent) => void,
    maxUploadBytesPerSecond: number | null,
  ) => WebRTCProxy;

  private workerClient: HashtreeWorkerClient | null = null;
  private desiredSession: ManagedWebRTCMeshSessionConfig | null = null;
  private active: ActiveMesh | null = null;
  private syncVersion = 0;
  private currentSync: Promise<void> | null = null;
  private uploadLimitBytesPerSecond: number | null = null;
  private poolConfig: WebRTCMeshPoolConfig | null = null;
  private healthCheckTimer: ReturnType<typeof setInterval> | null = null;
  private lastHealthyAt = 0;
  private lastHelloAt = 0;
  private lastRestartAt = 0;

  constructor(options: ManagedWebRTCMeshHostOptions = {}) {
    this.healthCheckIntervalMs = options.healthCheckIntervalMs ?? DEFAULT_HEALTH_CHECK_INTERVAL_MS;
    this.reannounceIntervalMs = options.reannounceIntervalMs ?? DEFAULT_REANNOUNCE_INTERVAL_MS;
    this.restartIntervalMs = options.restartIntervalMs ?? DEFAULT_RESTART_INTERVAL_MS;
    this.createSignalPool = options.createSignalPool ?? (() => new SimplePool({ enableReconnect: true }));
    this.createController = options.createController ?? ((config) => new WebRTCController(config));
    this.createProxy = options.createProxy ?? ((onEvent, maxUploadBytesPerSecond) => (
      new WebRTCProxy(onEvent, { maxUploadBytesPerSecond })
    ));
  }

  attachWorkerClient(
    client: HashtreeWorkerClient,
    options: { canFetch?: () => boolean | Promise<boolean> } = {},
  ): void {
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

  async setSession(
    session: ManagedWebRTCMeshSessionConfig | null,
    force = false,
  ): Promise<void> {
    this.desiredSession = session;
    const version = ++this.syncVersion;
    const syncPromise = this.runSync(version, force);
    this.currentSync = syncPromise;
    await syncPromise;
  }

  setUploadLimitBytesPerSecond(maxUploadBytesPerSecond?: number | null): void {
    this.uploadLimitBytesPerSecond = maxUploadBytesPerSecond ?? null;
    this.active?.proxy.setUploadLimitBytesPerSecond(this.uploadLimitBytesPerSecond);
  }

  setPoolConfig(config: WebRTCMeshPoolConfig | null): void {
    this.poolConfig = config;
    this.active?.controller.setPoolConfig(config);
  }

  broadcastHello(): void {
    this.lastHelloAt = Date.now();
    this.active?.controller.broadcastHello();
  }

  getController(): WebRTCController | null {
    return this.active?.controller ?? null;
  }

  getPubkey(): string | null {
    return this.active?.pubkey ?? null;
  }

  getActiveSignature(): string | null {
    return this.active?.signature ?? null;
  }

  getRelayUrls(): string[] {
    return this.active?.relayUrls.slice() ?? this.desiredSession?.relayUrls.slice() ?? [];
  }

  getRelayConnectionStatus(): ReadonlyMap<string, boolean> {
    return new Map(this.active?.signalPool.listConnectionStatus() ?? []);
  }

  getConnectedPeerCount(): number {
    return this.active?.controller.getConnectedCount() ?? 0;
  }

  getPeerStats(): ReturnType<WebRTCController['getPeerStats']> {
    return this.active?.controller.getPeerStats() ?? [];
  }

  isActive(): boolean {
    return this.active !== null;
  }

  async close(): Promise<void> {
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

  private async ensureStarted(): Promise<void> {
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

  private startHealthCheck(): void {
    if (this.healthCheckTimer || this.healthCheckIntervalMs <= 0) {
      return;
    }
    this.healthCheckTimer = setInterval(() => {
      this.checkHealth();
    }, this.healthCheckIntervalMs);
  }

  private stopHealthCheck(): void {
    if (!this.healthCheckTimer) {
      return;
    }
    clearInterval(this.healthCheckTimer);
    this.healthCheckTimer = null;
  }

  private checkHealth(): void {
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

  private async runSync(version: number, force: boolean): Promise<void> {
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

    let controller: WebRTCController | null = null;
    const signalPool = this.createSignalPool();
    (
      signalPool as SimplePool & {
        onRelayConnectionSuccess?: (url: string) => void;
      }
    ).onRelayConnectionSuccess = () => {
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

    const handleSignalEvent = (event: SignalingEventLike): void => {
      if (event.pubkey === session.pubkey) {
        return;
      }
      void this.routeSignalEvent(session, controller!, event);
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
    this.lastHealthyAt = Date.now();
    this.lastHelloAt = 0;
  }

  private async routeSignalEvent(
    session: ManagedWebRTCMeshSessionConfig,
    controller: WebRTCController,
    event: SignalingEventLike,
  ): Promise<void> {
    const decoded = await decodeSignalingEvent({
      event,
      giftUnwrap: session.unwrapGift,
    });

    if (!decoded || decoded.senderPubkey === session.pubkey) {
      return;
    }

    await controller.handleSignalingMessage(decoded.message, decoded.senderPubkey);
  }

  private async disposeActive(): Promise<void> {
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
