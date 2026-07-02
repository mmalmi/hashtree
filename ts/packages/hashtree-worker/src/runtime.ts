export interface HtreeRuntimeLocationLike {
  protocol?: string;
  hostname?: string;
  search?: string;
}

export interface HtreeRuntimeWindowLike {
  location?: HtreeRuntimeLocationLike;
  __HTREE_SERVER_URL__?: string;
  __HTREE_CANONICAL_URL__?: string | null;
  htree?: {
    htreeBaseUrl?: string;
  };
}

export interface ResolveRuntimeHtreeBaseUrlOptions {
  windowLike?: HtreeRuntimeWindowLike;
  fallbackBaseUrl?: string | null;
}

function getWindowLike(windowLike?: HtreeRuntimeWindowLike): HtreeRuntimeWindowLike | null {
  if (windowLike) return windowLike;
  if (typeof window === 'undefined') return null;
  return window as HtreeRuntimeWindowLike;
}

function normalizeBaseUrl(url: string | null | undefined): string {
  return (url ?? '').trim().replace(/\/$/, '');
}

function getQueryParam(name: string, windowLike?: HtreeRuntimeWindowLike): string | null {
  const runtimeWindow = getWindowLike(windowLike);
  if (!runtimeWindow) return null;
  try {
    const value = new URLSearchParams(runtimeWindow.location?.search ?? '').get(name);
    return typeof value === 'string' ? value.trim() || null : null;
  } catch {
    return null;
  }
}

function getPageProtocol(windowLike?: HtreeRuntimeWindowLike): string | null {
  const runtimeWindow = getWindowLike(windowLike);
  const protocol = runtimeWindow?.location?.protocol;
  return typeof protocol === 'string' ? protocol.toLowerCase() : null;
}

function getPageHostname(windowLike?: HtreeRuntimeWindowLike): string | null {
  const runtimeWindow = getWindowLike(windowLike);
  const hostname = runtimeWindow?.location?.hostname;
  return typeof hostname === 'string' ? hostname.toLowerCase() : null;
}

function isLoopbackHostname(hostname: string): boolean {
  return hostname === '127.0.0.1'
    || hostname === 'localhost';
}

function isBridgeRuntimeHostname(hostname: string): boolean {
  return hostname.endsWith('.htree.localhost')
    || hostname.endsWith('.iris.localhost');
}

function isLocalRuntimeHostname(hostname: string): boolean {
  return isLoopbackHostname(hostname) || isBridgeRuntimeHostname(hostname);
}

function hasCanonicalHtreeIdentity(windowLike?: HtreeRuntimeWindowLike): boolean {
  const runtimeWindow = getWindowLike(windowLike);
  const injectedCanonical = runtimeWindow?.__HTREE_CANONICAL_URL__;
  const canonical = typeof injectedCanonical === 'string' && injectedCanonical.trim()
    ? injectedCanonical.trim()
    : getQueryParam('htree_canonical', windowLike);
  return typeof canonical === 'string' && canonical.toLowerCase().startsWith('htree://');
}

function isLoopbackChildRuntime(windowLike?: HtreeRuntimeWindowLike): boolean {
  if (getPageProtocol(windowLike) !== 'http:') return false;
  const hostname = getPageHostname(windowLike);
  if (!hostname) return false;
  return isLocalRuntimeHostname(hostname);
}

function isBridgeChildRuntime(windowLike?: HtreeRuntimeWindowLike): boolean {
  if (getPageProtocol(windowLike) !== 'http:') return false;
  const hostname = getPageHostname(windowLike);
  if (!hostname) return false;
  return isBridgeRuntimeHostname(hostname);
}

function getServerProtocol(serverUrl: string): string | null {
  try {
    return new URL(serverUrl).protocol.toLowerCase();
  } catch {
    return null;
  }
}

function getServerHostname(serverUrl: string): string | null {
  try {
    return new URL(serverUrl).hostname.toLowerCase();
  } catch {
    return null;
  }
}

function isLocalHttpServerUrl(serverUrl: string): boolean {
  try {
    const parsed = new URL(serverUrl);
    const hostname = parsed.hostname.toLowerCase();
    return (parsed.protocol === 'http:' || parsed.protocol === 'https:')
      && isLocalRuntimeHostname(hostname);
  } catch {
    return false;
  }
}

function getWindowHtreeBaseUrl(windowLike?: HtreeRuntimeWindowLike): string {
  const runtimeWindow = getWindowLike(windowLike);
  return normalizeBaseUrl(runtimeWindow?.htree?.htreeBaseUrl);
}

export function getInjectedHtreeServerUrl(windowLike?: HtreeRuntimeWindowLike): string | null {
  const runtimeWindow = getWindowLike(windowLike);
  if (!runtimeWindow) return null;
  const override = runtimeWindow.__HTREE_SERVER_URL__;
  const fallback = getQueryParam('htree_server', runtimeWindow);
  const candidate = typeof override === 'string' && override.trim() ? override : fallback;
  const normalized = normalizeBaseUrl(candidate);
  return normalized || null;
}

export function shouldEagerLoadMediaInNativeChildRuntime(windowLike?: HtreeRuntimeWindowLike): boolean {
  return isLoopbackChildRuntime(windowLike) && hasCanonicalHtreeIdentity(windowLike);
}

export function canUseLocalHtreeRoutes(windowLike?: HtreeRuntimeWindowLike): boolean {
  return shouldEagerLoadMediaInNativeChildRuntime(windowLike) || isBridgeChildRuntime(windowLike);
}

export function shouldPreferSameOriginHtreeRoutes(windowLike?: HtreeRuntimeWindowLike): boolean {
  const serverUrl = getInjectedHtreeServerUrl(windowLike);
  if (!serverUrl) return false;
  const serverProtocol = getServerProtocol(serverUrl);
  if (serverProtocol !== 'http:') return false;

  const pageProtocol = getPageProtocol(windowLike);
  if (pageProtocol === 'https:') return true;
  if (pageProtocol === 'htree:') {
    const hostname = getPageHostname(windowLike);
    return hostname?.startsWith('npub1') === true || hostname === 'self';
  }
  if (hasCanonicalHtreeIdentity(windowLike) && !isLoopbackChildRuntime(windowLike)) return true;
  return false;
}

export function canUseInjectedHtreeServerUrl(windowLike?: HtreeRuntimeWindowLike): boolean {
  const serverUrl = getInjectedHtreeServerUrl(windowLike);
  return !!serverUrl && !shouldPreferSameOriginHtreeRoutes(windowLike);
}

export function canUseSameOriginHtreeProtocolStreaming(windowLike?: HtreeRuntimeWindowLike): boolean {
  return getPageProtocol(windowLike) === 'htree:';
}

export function resolveRuntimeHtreeBaseUrl(
  options: ResolveRuntimeHtreeBaseUrlOptions = {},
): string {
  const { windowLike, fallbackBaseUrl } = options;
  const injectedServerUrl = getInjectedHtreeServerUrl(windowLike);
  const windowBaseUrl = getWindowHtreeBaseUrl(windowLike);
  const canUseLocalRoutes = canUseLocalHtreeRoutes(windowLike);

  if (injectedServerUrl && canUseInjectedHtreeServerUrl(windowLike)) {
    return injectedServerUrl;
  }

  if (windowBaseUrl) {
    const windowBaseHostname = getServerHostname(windowBaseUrl);
    if (!isLocalHttpServerUrl(windowBaseUrl)) {
      return windowBaseUrl;
    }
    if (windowBaseHostname && isLoopbackHostname(windowBaseHostname)) {
      return windowBaseUrl;
    }
    if (windowBaseHostname && isBridgeRuntimeHostname(windowBaseHostname) && isBridgeChildRuntime(windowLike)) {
      return '';
    }
  }
  if (canUseLocalRoutes || isBridgeChildRuntime(windowLike)) {
    return '';
  }

  return normalizeBaseUrl(fallbackBaseUrl);
}
