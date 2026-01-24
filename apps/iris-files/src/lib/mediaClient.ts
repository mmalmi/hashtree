const MEDIA_CLIENT_KEY = 'htree.mediaClientId';
let cachedId: string | null = null;

function generateClientId(): string {
  if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
    return crypto.randomUUID();
  }
  return `mc_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 10)}`;
}

export function getMediaClientId(): string | null {
  if (cachedId) return cachedId;
  if (typeof window === 'undefined') return null;

  try {
    const existing = sessionStorage.getItem(MEDIA_CLIENT_KEY);
    if (existing) {
      cachedId = existing;
      return existing;
    }
  } catch {
    // ignore storage errors
  }

  const nextId = generateClientId();
  try {
    sessionStorage.setItem(MEDIA_CLIENT_KEY, nextId);
  } catch {
    // ignore storage errors
  }
  cachedId = nextId;
  return nextId;
}
