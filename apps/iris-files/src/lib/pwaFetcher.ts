/**
 * PWA Fetcher - crawls and saves PWA assets to hashtree
 *
 * Fetches a PWA's HTML, manifest, and all linked assets (CSS, JS, images, icons)
 * so they can be stored in a content-addressed hashtree for offline access.
 */

export interface PWAManifest {
  name?: string;
  short_name?: string;
  start_url?: string;
  display?: string;
  background_color?: string;
  theme_color?: string;
  icons?: Array<{
    src: string;
    sizes?: string;
    type?: string;
  }>;
}

export interface PWAAsset {
  path: string;
  data: Uint8Array;
  contentType: string;
}

export interface PWAInfo {
  url: string;
  manifest?: PWAManifest;
  assets: PWAAsset[];
}

/**
 * Parse HTML to extract linked resources
 */
function parseHtmlLinks(html: string, baseUrl: URL): {
  manifestUrl: string | null;
  stylesheets: string[];
  scripts: string[];
  images: string[];
} {
  const manifestUrl = extractManifestUrl(html, baseUrl);
  const stylesheets = extractLinkHrefs(html, 'stylesheet', baseUrl);
  const scripts = extractScriptSrcs(html, baseUrl);
  const images = extractImageSrcs(html, baseUrl);

  return { manifestUrl, stylesheets, scripts, images };
}

function extractManifestUrl(html: string, baseUrl: URL): string | null {
  // Match <link rel="manifest" href="...">
  const match = html.match(/<link[^>]+rel=["']manifest["'][^>]*>/i);
  if (!match) return null;

  const hrefMatch = match[0].match(/href=["']([^"']+)["']/i);
  if (!hrefMatch) return null;

  return new URL(hrefMatch[1], baseUrl).href;
}

function extractLinkHrefs(html: string, rel: string, baseUrl: URL): string[] {
  const regex = new RegExp(`<link[^>]+rel=["']${rel}["'][^>]*>`, 'gi');
  const links: string[] = [];

  let match;
  while ((match = regex.exec(html)) !== null) {
    const hrefMatch = match[0].match(/href=["']([^"']+)["']/i);
    if (hrefMatch) {
      try {
        links.push(new URL(hrefMatch[1], baseUrl).href);
      } catch {
        // Invalid URL, skip
      }
    }
  }

  return links;
}

function extractScriptSrcs(html: string, baseUrl: URL): string[] {
  const regex = /<script[^>]+src=["']([^"']+)["'][^>]*>/gi;
  const scripts: string[] = [];

  let match;
  while ((match = regex.exec(html)) !== null) {
    try {
      scripts.push(new URL(match[1], baseUrl).href);
    } catch {
      // Invalid URL, skip
    }
  }

  return scripts;
}

function extractImageSrcs(html: string, baseUrl: URL): string[] {
  const regex = /<img[^>]+src=["']([^"']+)["'][^>]*>/gi;
  const images: string[] = [];

  let match;
  while ((match = regex.exec(html)) !== null) {
    try {
      // Skip data: URLs
      if (!match[1].startsWith('data:')) {
        images.push(new URL(match[1], baseUrl).href);
      }
    } catch {
      // Invalid URL, skip
    }
  }

  return images;
}

/**
 * Extract URLs from CSS (background images, fonts, etc.)
 */
function extractCssUrls(css: string, baseUrl: URL): string[] {
  const regex = /url\(["']?([^"')]+)["']?\)/gi;
  const urls: string[] = [];

  let match;
  while ((match = regex.exec(css)) !== null) {
    try {
      // Skip data: URLs
      if (!match[1].startsWith('data:')) {
        urls.push(new URL(match[1], baseUrl).href);
      }
    } catch {
      // Invalid URL, skip
    }
  }

  return urls;
}

/**
 * Convert absolute URL to relative path for storage
 */
function urlToPath(url: string, baseUrl: URL): string {
  const urlObj = new URL(url);

  // Same origin - use pathname
  if (urlObj.origin === baseUrl.origin) {
    let path = urlObj.pathname;
    // Remove leading slash
    if (path.startsWith('/')) {
      path = path.slice(1);
    }
    return path || 'index.html';
  }

  // Different origin - store under _external/hostname/path
  return `_external/${urlObj.hostname}${urlObj.pathname}`;
}

/**
 * Get content type from response headers or guess from extension
 */
function getContentType(response: Response, url: string): string {
  const contentType = response.headers.get('content-type');
  if (contentType) {
    // Return just the mime type, not charset etc.
    return contentType.split(';')[0].trim();
  }

  // Guess from extension
  const ext = url.split('.').pop()?.toLowerCase();
  const mimeTypes: Record<string, string> = {
    html: 'text/html',
    css: 'text/css',
    js: 'application/javascript',
    json: 'application/json',
    png: 'image/png',
    jpg: 'image/jpeg',
    jpeg: 'image/jpeg',
    gif: 'image/gif',
    svg: 'image/svg+xml',
    webp: 'image/webp',
    ico: 'image/x-icon',
    woff: 'font/woff',
    woff2: 'font/woff2',
    ttf: 'font/ttf',
    eot: 'application/vnd.ms-fontobject',
  };

  return mimeTypes[ext || ''] || 'application/octet-stream';
}

/**
 * Fetch a single asset
 */
async function fetchAsset(url: string, baseUrl: URL): Promise<PWAAsset | null> {
  try {
    const response = await fetch(url, {
      credentials: 'omit',
      mode: 'cors',
    });

    if (!response.ok) {
      console.warn(`[PWA Fetcher] Failed to fetch ${url}: ${response.status}`);
      return null;
    }

    const data = new Uint8Array(await response.arrayBuffer());
    const contentType = getContentType(response, url);
    const path = urlToPath(url, baseUrl);

    return { path, data, contentType };
  } catch (error) {
    console.warn(`[PWA Fetcher] Error fetching ${url}:`, error);
    return null;
  }
}

/**
 * Rewrite URLs in HTML to use relative paths
 */
function rewriteHtmlUrls(html: string, baseUrl: URL): string {
  let result = html;

  // Rewrite manifest link
  result = result.replace(
    /(<link[^>]+rel=["']manifest["'][^>]+href=["'])([^"']+)(["'][^>]*>)/gi,
    (_, prefix, href, suffix) => {
      try {
        const absUrl = new URL(href, baseUrl).href;
        const relPath = urlToPath(absUrl, baseUrl);
        return `${prefix}${relPath}${suffix}`;
      } catch {
        return _;
      }
    }
  );

  // Rewrite stylesheet links
  result = result.replace(
    /(<link[^>]+href=["'])([^"']+)(["'][^>]*>)/gi,
    (match, prefix, href, suffix) => {
      // Skip non-stylesheet links (unless it's manifest which we already handled)
      if (!match.includes('stylesheet')) return match;
      try {
        const absUrl = new URL(href, baseUrl).href;
        const relPath = urlToPath(absUrl, baseUrl);
        return `${prefix}${relPath}${suffix}`;
      } catch {
        return match;
      }
    }
  );

  // Rewrite script srcs
  result = result.replace(
    /(<script[^>]+src=["'])([^"']+)(["'][^>]*>)/gi,
    (_, prefix, src, suffix) => {
      try {
        const absUrl = new URL(src, baseUrl).href;
        const relPath = urlToPath(absUrl, baseUrl);
        return `${prefix}${relPath}${suffix}`;
      } catch {
        return _;
      }
    }
  );

  // Rewrite image srcs
  result = result.replace(
    /(<img[^>]+src=["'])([^"']+)(["'][^>]*>)/gi,
    (_, prefix, src, suffix) => {
      if (src.startsWith('data:')) return _;
      try {
        const absUrl = new URL(src, baseUrl).href;
        const relPath = urlToPath(absUrl, baseUrl);
        return `${prefix}${relPath}${suffix}`;
      } catch {
        return _;
      }
    }
  );

  // Remove service worker registration
  result = result.replace(
    /<script[^>]*>[^<]*navigator\.serviceWorker\.register[^<]*<\/script>/gi,
    ''
  );

  return result;
}

/**
 * Rewrite URLs in CSS to use relative paths
 */
function rewriteCssUrls(css: string, cssUrl: string, baseUrl: URL): string {
  const cssBaseUrl = new URL(cssUrl);

  return css.replace(
    /url\(["']?([^"')]+)["']?\)/gi,
    (match, url) => {
      if (url.startsWith('data:')) return match;
      try {
        const absUrl = new URL(url, cssBaseUrl).href;
        const relPath = urlToPath(absUrl, baseUrl);
        // Calculate relative path from CSS file location
        const cssPath = urlToPath(cssUrl, baseUrl);
        const cssDir = cssPath.split('/').slice(0, -1).join('/');
        const relativePath = getRelativePath(cssDir, relPath);
        return `url("${relativePath}")`;
      } catch {
        return match;
      }
    }
  );
}

/**
 * Get relative path from one path to another
 */
function getRelativePath(from: string, to: string): string {
  const fromParts = from.split('/').filter(Boolean);
  const toParts = to.split('/').filter(Boolean);

  // Find common prefix
  let common = 0;
  while (common < fromParts.length && common < toParts.length && fromParts[common] === toParts[common]) {
    common++;
  }

  // Build relative path
  const ups = fromParts.length - common;
  const downs = toParts.slice(common);

  const parts: string[] = [];
  for (let i = 0; i < ups; i++) {
    parts.push('..');
  }
  parts.push(...downs);

  return parts.join('/') || '.';
}

/**
 * Rewrite URLs in manifest to use relative paths
 */
function rewriteManifestUrls(manifest: PWAManifest, manifestUrl: string, baseUrl: URL): PWAManifest {
  const result = { ...manifest };

  if (result.start_url) {
    try {
      const absUrl = new URL(result.start_url, manifestUrl).href;
      result.start_url = urlToPath(absUrl, baseUrl);
      // Make it relative to manifest location
      if (result.start_url === 'index.html') {
        result.start_url = './';
      }
    } catch {
      result.start_url = './';
    }
  }

  if (result.icons) {
    result.icons = result.icons.map(icon => ({
      ...icon,
      src: urlToPath(new URL(icon.src, manifestUrl).href, baseUrl),
    }));
  }

  return result;
}

/**
 * Fetch and crawl a PWA, returning all assets
 */
export async function fetchPWA(url: string): Promise<PWAInfo> {
  const baseUrl = new URL(url);
  const assets: PWAAsset[] = [];
  const fetchedUrls = new Set<string>();

  // Fetch main HTML
  const htmlResponse = await fetch(url, {
    credentials: 'omit',
    mode: 'cors',
  });

  if (!htmlResponse.ok) {
    throw new Error(`Failed to fetch PWA: ${htmlResponse.status}`);
  }

  const originalHtml = await htmlResponse.text();
  const links = parseHtmlLinks(originalHtml, baseUrl);

  // Track manifest info
  let manifest: PWAManifest | undefined;

  // Fetch manifest if present
  if (links.manifestUrl && !fetchedUrls.has(links.manifestUrl)) {
    fetchedUrls.add(links.manifestUrl);
    try {
      const manifestResponse = await fetch(links.manifestUrl, {
        credentials: 'omit',
        mode: 'cors',
      });

      if (manifestResponse.ok) {
        const manifestData = await manifestResponse.json();
        manifest = manifestData;

        // Rewrite manifest URLs
        const rewrittenManifest = rewriteManifestUrls(manifest, links.manifestUrl, baseUrl);

        // Store rewritten manifest
        const manifestPath = urlToPath(links.manifestUrl, baseUrl);
        assets.push({
          path: manifestPath,
          data: new TextEncoder().encode(JSON.stringify(rewrittenManifest, null, 2)),
          contentType: 'application/json',
        });

        // Add manifest icons to fetch list
        if (manifest.icons) {
          for (const icon of manifest.icons) {
            const iconUrl = new URL(icon.src, links.manifestUrl).href;
            if (!fetchedUrls.has(iconUrl)) {
              links.images.push(iconUrl);
            }
          }
        }
      }
    } catch (error) {
      console.warn('[PWA Fetcher] Failed to fetch manifest:', error);
    }
  }

  // Fetch stylesheets and extract their URLs
  const cssUrls: string[] = [];
  for (const cssUrl of links.stylesheets) {
    if (fetchedUrls.has(cssUrl)) continue;
    fetchedUrls.add(cssUrl);

    try {
      const response = await fetch(cssUrl, {
        credentials: 'omit',
        mode: 'cors',
      });

      if (response.ok) {
        const cssText = await response.text();

        // Extract additional URLs from CSS
        const cssResourceUrls = extractCssUrls(cssText, new URL(cssUrl));
        cssUrls.push(...cssResourceUrls);

        // Rewrite URLs in CSS
        const rewrittenCss = rewriteCssUrls(cssText, cssUrl, baseUrl);

        assets.push({
          path: urlToPath(cssUrl, baseUrl),
          data: new TextEncoder().encode(rewrittenCss),
          contentType: 'text/css',
        });
      }
    } catch (error) {
      console.warn(`[PWA Fetcher] Failed to fetch CSS ${cssUrl}:`, error);
    }
  }

  // Add CSS resource URLs to images list
  for (const url of cssUrls) {
    if (!fetchedUrls.has(url) && !links.images.includes(url)) {
      links.images.push(url);
    }
  }

  // Fetch scripts
  for (const scriptUrl of links.scripts) {
    if (fetchedUrls.has(scriptUrl)) continue;
    fetchedUrls.add(scriptUrl);

    const asset = await fetchAsset(scriptUrl, baseUrl);
    if (asset) {
      assets.push(asset);
    }
  }

  // Fetch images
  for (const imageUrl of links.images) {
    if (fetchedUrls.has(imageUrl)) continue;
    fetchedUrls.add(imageUrl);

    const asset = await fetchAsset(imageUrl, baseUrl);
    if (asset) {
      assets.push(asset);
    }
  }

  // Rewrite HTML URLs and store
  const rewrittenHtml = rewriteHtmlUrls(originalHtml, baseUrl);
  assets.unshift({
    path: 'index.html',
    data: new TextEncoder().encode(rewrittenHtml),
    contentType: 'text/html',
  });

  return {
    url,
    manifest,
    assets,
  };
}

/**
 * Quick check if a URL has a PWA manifest
 */
export async function detectPWA(url: string): Promise<PWAManifest | null> {
  try {
    const response = await fetch(url, {
      credentials: 'omit',
      mode: 'cors',
    });

    if (!response.ok) return null;

    const html = await response.text();
    const baseUrl = new URL(url);
    const manifestUrl = extractManifestUrl(html, baseUrl);

    if (!manifestUrl) return null;

    const manifestResponse = await fetch(manifestUrl, {
      credentials: 'omit',
      mode: 'cors',
    });

    if (!manifestResponse.ok) return null;

    return await manifestResponse.json();
  } catch {
    return null;
  }
}
