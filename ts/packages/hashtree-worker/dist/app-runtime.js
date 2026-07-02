import { appendHtreeClientId, appendHtreeQueryParam, getOrCreateHtreeClientId, } from './client-id.js';
import { resolveHtreeRequestUrl } from './htree-url.js';
import { canUseInjectedHtreeServerUrl, canUseLocalHtreeRoutes, canUseSameOriginHtreeProtocolStreaming, } from './runtime.js';
import { resolveRuntimeEndpoints } from './runtime-network.js';
const DEFAULT_FALLBACK_BASE_URL = '';
const DEFAULT_MEDIA_PORT_ATTEMPTS = 3;
const DEFAULT_MEDIA_PORT_DELAY_MS = 500;
const DEFAULT_MEDIA_PORT_PING_TIMEOUT_MS = 1_500;
const DEFAULT_MEDIA_PORT_REGISTRATION_TIMEOUT_MS = 5_000;
const DEFAULT_MEDIA_PORT_CONTROLLER_TIMEOUT_MS = 5_000;
const RECONNECT_REQUEST_COOLDOWN_MS = 1_000;
function resolveRuntimeValue(value, fallback) {
    if (typeof value === 'function') {
        return value();
    }
    return value ?? fallback;
}
function getServiceWorkerContainer(serviceWorker) {
    if (typeof serviceWorker !== 'undefined') {
        return serviceWorker;
    }
    if (typeof navigator === 'undefined') {
        return null;
    }
    return navigator.serviceWorker ?? null;
}
function isDirectMediaRuntime(windowLike) {
    return canUseInjectedHtreeServerUrl(windowLike)
        || canUseSameOriginHtreeProtocolStreaming(windowLike)
        || canUseLocalHtreeRoutes(windowLike);
}
function createMessageId(prefix) {
    return `${prefix}-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}
export function createHtreeRuntime(options = {}) {
    const appId = options.appId?.trim() || null;
    const fallbackBaseUrl = options.fallbackBaseUrl ?? DEFAULT_FALLBACK_BASE_URL;
    const windowLike = options.windowLike;
    const storage = typeof options.storage === 'undefined' ? undefined : options.storage;
    const storageKey = options.clientIdStorageKey ?? (appId ? `${appId}.mediaClientId` : 'htree.mediaClientId');
    const clientIdPrefix = options.clientIdPrefix ?? (appId ? appId.replace(/[^a-z0-9]+/gi, '').toLowerCase() || 'htree' : 'htree');
    const serviceWorker = getServiceWorkerContainer(options.serviceWorker);
    let setupPromise = null;
    let mediaReady = false;
    let activeController = null;
    let controllerListenerAttached = false;
    let messageListenerAttached = false;
    let reconnectPromise = null;
    let lastReconnectRequestAt = 0;
    let lastEnsureOptions = null;
    const getClientId = () => getOrCreateHtreeClientId({
        storage,
        storageKey,
        prefix: clientIdPrefix,
        uuidFactory: options.clientIdFactory,
    });
    const getEndpoints = (overrides = {}) => {
        const relays = overrides.relays ?? resolveRuntimeValue(options.relays, []);
        const blossomServers = overrides.blossomServers ?? resolveRuntimeValue(options.blossomServers, []);
        return resolveRuntimeEndpoints({
            windowLike,
            relays,
            blossomServers,
        });
    };
    const appendRuntimeClientId = (url) => appendHtreeClientId(url, getClientId());
    const resolveRequestUrl = (input, urlOptions = {}) => resolveHtreeRequestUrl(input, {
        ...urlOptions,
        windowLike,
        fallbackBaseUrl,
    });
    const resolveMediaUrl = (input, mediaOptions = {}) => {
        let url = resolveRequestUrl(input, mediaOptions);
        if (mediaOptions.clientScoped) {
            url = appendRuntimeClientId(url);
        }
        url = appendHtreeQueryParam(url, 'htree_t', mediaOptions.mimeType);
        for (const [key, value] of Object.entries(mediaOptions.query ?? {})) {
            url = appendHtreeQueryParam(url, key, value == null ? value : String(value));
        }
        return url;
    };
    const getWorkerConfig = (configOptions = {}) => {
        const { relays, blossomServers, ...rest } = configOptions;
        const endpoints = getEndpoints({ relays, blossomServers });
        return {
            ...rest,
            relays: endpoints.nostrRelays,
            blossomServers: endpoints.blossomServers,
        };
    };
    const resetMedia = () => {
        mediaReady = false;
        setupPromise = null;
        activeController = null;
    };
    const ensureControllerListener = () => {
        if (controllerListenerAttached || !serviceWorker)
            return;
        controllerListenerAttached = true;
        serviceWorker.addEventListener('controllerchange', () => {
            resetMedia();
        });
    };
    const requestMediaReconnect = (requestedClientKey) => {
        const clientId = getClientId();
        if (!lastEnsureOptions)
            return;
        if (requestedClientKey && clientId && requestedClientKey !== clientId) {
            return;
        }
        const now = Date.now();
        if (reconnectPromise || now - lastReconnectRequestAt < RECONNECT_REQUEST_COOLDOWN_MS) {
            return;
        }
        lastReconnectRequestAt = now;
        resetMedia();
        reconnectPromise = ensureMediaPortReady(lastEnsureOptions)
            .then(() => undefined)
            .catch(() => undefined)
            .finally(() => {
            reconnectPromise = null;
        });
    };
    const ensureMessageListener = () => {
        if (messageListenerAttached || !serviceWorker)
            return;
        messageListenerAttached = true;
        serviceWorker.addEventListener('message', (event) => {
            const data = event.data;
            if (data?.type !== 'REQUEST_WORKER_PORT_RECONNECT') {
                return;
            }
            requestMediaReconnect(data.clientKey ?? null);
        });
    };
    const waitForController = async (timeoutMs) => {
        if (!serviceWorker)
            return null;
        if (serviceWorker.controller)
            return serviceWorker.controller;
        await serviceWorker.ready.catch(() => undefined);
        if (serviceWorker.controller)
            return serviceWorker.controller;
        return await new Promise((resolve) => {
            const timeoutId = setTimeout(() => resolve(serviceWorker.controller ?? null), timeoutMs);
            serviceWorker.addEventListener('controllerchange', () => {
                clearTimeout(timeoutId);
                resolve(serviceWorker.controller ?? null);
            }, { once: true });
        });
    };
    const pingMediaPort = async (clientKey, controller, timeoutMs) => {
        if (!serviceWorker)
            return false;
        const requestId = createMessageId('media-ping');
        const ackPromise = new Promise((resolve) => {
            const timeoutId = setTimeout(() => {
                serviceWorker.removeEventListener('message', onMessage);
                resolve(false);
            }, timeoutMs);
            const onMessage = (event) => {
                const data = event.data;
                if (data?.type === 'WORKER_PORT_PONG' && data.requestId === requestId) {
                    clearTimeout(timeoutId);
                    serviceWorker.removeEventListener('message', onMessage);
                    resolve(!!data.ok);
                }
            };
            serviceWorker.addEventListener('message', onMessage);
        });
        controller.postMessage({ type: 'PING_WORKER_PORT', requestId, clientKey });
        return await ackPromise;
    };
    const setupMediaPort = async (portOptions) => {
        if (!serviceWorker) {
            return false;
        }
        ensureControllerListener();
        ensureMessageListener();
        const controller = await waitForController(portOptions.controllerTimeoutMs ?? DEFAULT_MEDIA_PORT_CONTROLLER_TIMEOUT_MS);
        if (!controller) {
            return false;
        }
        const clientKey = getClientId() ?? undefined;
        if (mediaReady && activeController === controller) {
            if (!clientKey) {
                return true;
            }
            const alive = await pingMediaPort(clientKey, controller, portOptions.pingTimeoutMs ?? DEFAULT_MEDIA_PORT_PING_TIMEOUT_MS);
            if (alive) {
                return true;
            }
            resetMedia();
        }
        const channel = new MessageChannel();
        const requestId = createMessageId('media');
        const ackPromise = new Promise((resolve) => {
            const timeoutId = setTimeout(() => {
                serviceWorker.removeEventListener('message', onMessage);
                resolve(false);
            }, portOptions.registrationTimeoutMs ?? DEFAULT_MEDIA_PORT_REGISTRATION_TIMEOUT_MS);
            const onMessage = (event) => {
                const data = event.data;
                if (data?.type === 'WORKER_PORT_READY' && data.requestId === requestId) {
                    clearTimeout(timeoutId);
                    serviceWorker.removeEventListener('message', onMessage);
                    resolve(true);
                }
            };
            serviceWorker.addEventListener('message', onMessage);
        });
        controller.postMessage({
            type: 'REGISTER_WORKER_PORT',
            port: channel.port1,
            requestId,
            clientKey,
            debug: !!portOptions.debug,
        }, [channel.port1]);
        await portOptions.registerMediaPort(channel.port2, !!portOptions.debug);
        const acked = await ackPromise;
        mediaReady = acked;
        activeController = acked ? controller : null;
        return acked;
    };
    const ensureMediaPortReady = async (portOptions) => {
        lastEnsureOptions = portOptions;
        if (isDirectMediaRuntime(windowLike)) {
            return true;
        }
        const attempts = portOptions.attempts ?? DEFAULT_MEDIA_PORT_ATTEMPTS;
        const delayMs = portOptions.delayMs ?? DEFAULT_MEDIA_PORT_DELAY_MS;
        for (let attempt = 0; attempt < attempts; attempt += 1) {
            if (!setupPromise) {
                setupPromise = setupMediaPort(portOptions).finally(() => {
                    if (!mediaReady) {
                        setupPromise = null;
                    }
                });
            }
            const ready = await setupPromise.catch(() => false);
            if (ready) {
                return true;
            }
            if (attempt < attempts - 1) {
                await new Promise((resolve) => setTimeout(resolve, delayMs));
            }
        }
        return false;
    };
    return {
        appId,
        get clientId() {
            return getClientId();
        },
        get endpoints() {
            return getEndpoints();
        },
        getEndpoints,
        getWorkerConfig,
        urls: {
            request: resolveRequestUrl,
            media: resolveMediaUrl,
            appendClientId: appendRuntimeClientId,
        },
        media: {
            ensureReady: ensureMediaPortReady,
            reset: resetMedia,
        },
    };
}
//# sourceMappingURL=app-runtime.js.map