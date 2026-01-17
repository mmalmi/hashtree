/**
 * Image proxy utility for proxying external images through imgproxy
 * Used for avatars, banners, and other external images
 * Based on iris-client implementation
 */

export interface ImgProxyConfig {
  url: string;
  key: string;
  salt: string;
}

export interface ImgProxyOptions {
  width?: number;
  height?: number;
  /** If true, use fill mode (crop to fill); if false, use fit mode (contain) */
  square?: boolean;
}

// Default imgproxy configuration (same as iris-client)
export const DEFAULT_IMGPROXY_CONFIG: ImgProxyConfig = {
  url: 'https://imgproxy.iris.to',
  key: 'f66233cb160ea07078ff28099bfa3e3e654bc10aa4a745e12176c433d79b8996',
  salt: '5e608e60945dcd2a787e8465d76ba34149894765061d39287609fb9d776caa0c',
};

// URL-safe base64 encoding
function urlSafeBase64(bytes: Uint8Array): string {
  const binString = Array.from(bytes, (byte) => String.fromCodePoint(byte)).join('');
  return btoa(binString).replace(/=/g, '').replace(/\+/g, '-').replace(/\//g, '_');
}

function hexToBytes(hex: string): Uint8Array {
  const bytes = new Uint8Array(hex.length / 2);
  for (let i = 0; i < bytes.length; i++) {
    bytes[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  }
  return bytes;
}

function concatBytes(...arrays: Uint8Array[]): Uint8Array {
  const totalLength = arrays.reduce((sum, arr) => sum + arr.length, 0);
  const result = new Uint8Array(totalLength);
  let offset = 0;
  for (const arr of arrays) {
    result.set(arr, offset);
    offset += arr.length;
  }
  return result;
}

// Sign the path using HMAC-SHA256 with Web Crypto API
async function signUrl(path: string, key: string, salt: string): Promise<string> {
  const te = new TextEncoder();
  const keyBytes = hexToBytes(key);
  const saltBytes = hexToBytes(salt);
  const pathBytes = te.encode(path);
  const data = concatBytes(saltBytes, pathBytes);

  const cryptoKey = await crypto.subtle.importKey(
    'raw',
    keyBytes,
    { name: 'HMAC', hash: 'SHA-256' },
    false,
    ['sign']
  );

  const signature = await crypto.subtle.sign('HMAC', cryptoKey, data);
  return urlSafeBase64(new Uint8Array(signature));
}

// Cache for generated URLs to avoid async overhead on repeated calls
const urlCache = new Map<string, string>();

/**
 * Generate a proxied image URL (async version)
 * @param originalSrc Original image URL
 * @param options Resize options
 * @param config Custom imgproxy config (optional)
 * @returns Proxied URL or original if generation fails
 */
export async function generateProxyUrlAsync(
  originalSrc: string,
  options: ImgProxyOptions = {},
  config: ImgProxyConfig = DEFAULT_IMGPROXY_CONFIG
): Promise<string> {
  try {
    // Skip if already proxied or is a data URL or blob URL
    if (
      originalSrc.startsWith(config.url) ||
      originalSrc.startsWith('data:') ||
      originalSrc.startsWith('blob:')
    ) {
      return originalSrc;
    }

    // Skip if not a valid URL
    try {
      new URL(originalSrc);
    } catch {
      return originalSrc;
    }

    // Build cache key
    const cacheKey = `${originalSrc}:${options.width}:${options.height}:${options.square}`;
    const cached = urlCache.get(cacheKey);
    if (cached) return cached;

    const te = new TextEncoder();
    const encodedUrl = urlSafeBase64(te.encode(originalSrc));

    const opts: string[] = [];
    if (options.width || options.height) {
      const resizeType = options.square ? 'fill' : 'fit';
      const w = options.width || options.height!;
      const h = options.height || options.width!;
      opts.push(`rs:${resizeType}:${w}:${h}`);
      opts.push('dpr:2');
    } else {
      opts.push('dpr:2');
    }

    const path = `/${opts.join('/')}/${encodedUrl}`;
    const signature = await signUrl(path, config.key, config.salt);
    const result = `${config.url}/${signature}${path}`;

    // Cache for future use
    urlCache.set(cacheKey, result);

    return result;
  } catch (e) {
    console.error('Failed to generate proxy URL:', e);
    return originalSrc;
  }
}

/**
 * Generate a proxied image URL (sync version - returns original if not cached)
 * Use this in render functions where async is not possible.
 * Call generateProxyUrlAsync first to populate the cache.
 * @param originalSrc Original image URL
 * @param options Resize options
 * @param config Custom imgproxy config (optional)
 * @returns Proxied URL from cache, or original if not cached yet
 */
export function generateProxyUrl(
  originalSrc: string,
  options: ImgProxyOptions = {},
  config: ImgProxyConfig = DEFAULT_IMGPROXY_CONFIG
): string {
  // Skip if already proxied or is a data URL or blob URL
  if (
    originalSrc.startsWith(config.url) ||
    originalSrc.startsWith('data:') ||
    originalSrc.startsWith('blob:')
  ) {
    return originalSrc;
  }

  // Skip if not a valid URL
  try {
    new URL(originalSrc);
  } catch {
    return originalSrc;
  }

  // Check cache
  const cacheKey = `${originalSrc}:${options.width}:${options.height}:${options.square}`;
  const cached = urlCache.get(cacheKey);
  if (cached) return cached;

  // Not in cache - trigger async generation and return original for now
  generateProxyUrlAsync(originalSrc, options, config);
  return originalSrc;
}
