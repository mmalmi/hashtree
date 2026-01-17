/**
 * Tauri E2E test for video source loading
 *
 * Verifies that:
 * 1. window.htree API is available with correct Tauri values
 * 2. htree:// protocol or HTTP server is running and accessible
 * 3. Video elements get correct http://127.0.0.1:21417/htree/... src URLs
 * 4. Videos actually load and play
 */
import { browser, $ } from '@wdio/globals';
import * as fs from 'fs';
import * as path from 'path';

const SCREENSHOTS_DIR = path.resolve(process.cwd(), 'e2e-tauri/screenshots');

if (!fs.existsSync(SCREENSHOTS_DIR)) {
  fs.mkdirSync(SCREENSHOTS_DIR, { recursive: true });
}

async function takeScreenshot(name: string): Promise<string> {
  const screenshot = await browser.takeScreenshot();
  const filepath = path.join(SCREENSHOTS_DIR, `${name}-${Date.now()}.png`);
  fs.writeFileSync(filepath, screenshot, 'base64');
  console.log(`Screenshot saved: ${filepath}`);
  return filepath;
}

const TEST_NHASH_VIDEO =
  'nhash1qqsd9gydzfdk3pmycjp6fvgzx3mur85s36gdpqjjmz4f909y5e98unq9ypanesh4tp629wy9at5l2t6c4vjwnpldmncd3x02e8t0wm3x4xx9ywfqt62';
const TEST_PROD_NPUB =
  'npub1l66ntjjz0aatw4g6mlzlesu3x2cse0q3eevyruz43p5qa985uudqqfaykt';
const TEST_PROD_TREE_NAME_ENCODED =
  'videos%2FTaistelukentt%C3%A4%202020%20%EF%BD%9C%20Slagf%C3%A4lt%202020%20%EF%BD%9C%20Battlefield%202020';
const APP_INDEX_URL = 'tauri://localhost/index.html#/';
const DEBUG_QUERY = 'htree_debug=1';
let mainHandle: string | null = null;
let videoHandle: string | null = null;

function isVideoWebviewUrl(url: string): boolean {
  try {
    return new URL(url).pathname.endsWith('/video.html');
  } catch {
    return url.includes('/video.html') && !url.includes('/index.html');
  }
}

async function recordMainHandle(): Promise<void> {
  if (!mainHandle) {
    mainHandle = await browser.getWindowHandle();
  }
}

async function switchToMain(): Promise<void> {
  await recordMainHandle();
  if (mainHandle) {
    await browser.switchToWindow(mainHandle);
  }
}

async function switchToVideoWebview(): Promise<void> {
  await recordMainHandle();
  if (videoHandle) {
    await browser.switchToWindow(videoHandle);
    return;
  }

  let foundHandle: string | null = null;
  await browser.waitUntil(
    async () => {
      const handles = await browser.getWindowHandles();
      for (const handle of handles) {
        try {
          await browser.switchToWindow(handle);
          const url = await browser.getUrl();
          if (isVideoWebviewUrl(url)) {
            foundHandle = handle;
            return true;
          }
        } catch {
          // Ignore transient handle issues while the webview initializes.
        }
      }
      return false;
    },
    {
      timeout: 30000,
      interval: 500,
      timeoutMsg: 'Video webview not found (child webview not ready)',
    }
  );

  if (!foundHandle) {
    await switchToMain();
    throw new Error('Video webview not found');
  }

  videoHandle = foundHandle;
  await browser.switchToWindow(videoHandle);
}

async function ensureAppLoaded(): Promise<void> {
  const currentUrl = await browser.getUrl();
  if (!currentUrl.startsWith('tauri://localhost/')) {
    await browser.url(APP_INDEX_URL);
  }

  await browser.waitUntil(
    async () => {
      return browser.execute(() => {
        const appRoot = document.getElementById('app');
        return !!appRoot && appRoot.children.length > 0;
      });
    },
    {
      timeout: 30000,
      timeoutMsg: 'App did not render after 30s',
      interval: 500,
    }
  );
}

async function enableHtreeDebug(): Promise<void> {
  await browser.execute(() => {
    (window as any).__HTREE_DEBUG__ = true;
    if (!(window as any).__HTREE_DEBUG_LOG__) {
      (window as any).__HTREE_DEBUG_LOG__ = [];
    }
    try {
      localStorage.setItem('htree.debug', '1');
    } catch {}
  });
}

async function forceVideoSettings(): Promise<void> {
  await browser.execute(() => {
    try {
      localStorage.setItem('video-settings', JSON.stringify({
        theaterMode: false,
        volume: 1,
        muted: false,
      }));
    } catch {}
  });
}

async function waitForHtreeApi(): Promise<void> {
  await ensureAppLoaded();
  await enableHtreeDebug();
  await browser.waitUntil(
    async () => {
      const ready = await browser.execute(() => {
        const htree = (window as any).htree;
        return !!htree?.htreeBaseUrl;
      });
      return ready;
    },
    {
      timeout: 30000,
      timeoutMsg: 'window.htree API not available after 30s',
      interval: 500,
    }
  );
}

describe('Video source loading in Tauri', () => {
  it('should have window.htree API initialized', async () => {
    await switchToMain();
    await waitForHtreeApi();
    const htreeApi = await browser.execute(() => {
      const htree = (window as any).htree;
      if (!htree || !htree.htreeBaseUrl) return null;
      return {
        version: htree.version,
        isTauri: htree.isTauri,
        htreeBaseUrl: htree.htreeBaseUrl,
        hasNostr: !!htree.nostr,
        hasDetectLocalRelay: typeof htree.detectLocalRelay === 'function',
        npub: htree.npub,
        isLoggedIn: htree.isLoggedIn,
      };
    });

    // API should exist
    expect(htreeApi).not.toBeNull();
    expect(htreeApi!.version).toBe('1.0.0');

    // In Tauri, isTauri should be true
    expect(htreeApi!.isTauri).toBe(true);

    // htreeBaseUrl should be the local server URL
    expect(htreeApi!.htreeBaseUrl).toMatch(/^http:\/\/(localhost|127\.0\.0\.1):21417$/);

    // detectLocalRelay should be a function
    expect(htreeApi!.hasDetectLocalRelay).toBe(true);
  });

  it('should have htree HTTP server running', async () => {
    await switchToMain();
    await waitForHtreeApi();
    const htreeBaseUrl = await browser.execute(() => {
      const htree = (window as any).htree;
      return htree?.htreeBaseUrl || null;
    });

    expect(htreeBaseUrl).not.toBeNull();

    // For custom htree:// protocol, fetch may not work (protocol handlers don't support fetch)
    // For http:// URLs, we can verify the server is running
    if ((htreeBaseUrl as string).startsWith('http://')) {
      const serverCheck = await browser.execute(async (baseUrl: string) => {
        try {
          const response = await fetch(`${baseUrl}/htree/test`, { method: 'HEAD' });
          return {
            ok: true,
            status: response.status,
            serverReachable: true,
          };
        } catch (e) {
          return {
            ok: false,
            error: String(e),
            serverReachable: false,
          };
        }
      }, htreeBaseUrl);

      expect(serverCheck.serverReachable).toBe(true);
    }
  });

  it('should navigate to video app from suggested apps', async () => {
    await switchToMain();
    await browser.url('tauri://localhost/index.html#/');
    await browser.waitUntil(
      async () => {
        const text = await browser.execute(() => document.body?.textContent || '');
        return text.includes('Suggestions');
      },
      {
        timeout: 30000,
        interval: 500,
        timeoutMsg: 'Home suggestions did not load',
      }
    );

    // Click the Iris Video suggestion (real user path, no direct URL rewrite).
    const clickResult = await browser.execute(() => {
      const headers = Array.from(document.querySelectorAll('h2'));
      const suggestionsHeader = headers.find((header) =>
        (header.textContent || '').toLowerCase().includes('suggestions')
      );
      const suggestionsSection = suggestionsHeader?.closest('section') ?? document.body;
      const buttons = Array.from(suggestionsSection.querySelectorAll('button'));
      const target = buttons.find((button) =>
        (button.textContent || '').toLowerCase().includes('iris video')
      );
      if (!target) {
        return {
          clicked: false,
          buttonTexts: buttons.map((button) => (button.textContent || '').trim()).filter(Boolean),
        };
      }
      target.click();
      return { clicked: true };
    });
    expect(clickResult.clicked).toBe(true);

    const addressInput = await $('input[placeholder="Search or enter address"]');
    await addressInput.waitForExist({ timeout: 30000 });

    await browser.waitUntil(
      async () => {
        const url = await browser.getUrl();
        const addressValue = await addressInput.getValue();
        return url.includes('/app/') && (url.includes('video.html') || addressValue.includes('video.html'));
      },
      {
        timeout: 30000,
        interval: 500,
        timeoutMsg: 'Video app did not open from suggestions',
      }
    );

    await switchToVideoWebview();
  });

  it('should load video with correct htree URL and verify it plays', async () => {
    await switchToVideoWebview();
    await enableHtreeDebug();
    // Navigate directly to a known content-addressed video (stable nhash)
    await browser.execute((nhash: string) => {
      window.location.hash = `#/${nhash}`;
    }, TEST_NHASH_VIDEO);

    const getVideoState = async () => {
      return browser.execute(() => {
        const video = document.querySelector('video') as HTMLVideoElement | null;
        if (!video) return null;
        const src = video.currentSrc || video.src || '';
        return {
          src,
          readyState: video.readyState,
          networkState: video.networkState,
          videoWidth: video.videoWidth,
          videoHeight: video.videoHeight,
          duration: video.duration,
          error: video.error ? { code: video.error.code, message: video.error.message } : null,
        };
      });
    };

    await browser.waitUntil(
      async () => {
        const result = await getVideoState();
        return !!result?.src;
      },
      {
        timeout: 60000,
        timeoutMsg: 'Video element with src not found after 60s',
        interval: 500,
      }
    );

    await browser.waitUntil(
      async () => {
        const result = await getVideoState();
        return !!result?.src && result.readyState >= 2 && result.videoWidth > 0;
      },
      {
        timeout: 60000,
        timeoutMsg: 'Video element did not load media data after 60s',
        interval: 500,
      }
    );

    const videoState = await getVideoState();
    if (!videoState) {
      throw new Error('Video element did not expose a src after wait');
    }
    expect(videoState.error).toBeNull();
    expect(videoState.readyState).toBeGreaterThanOrEqual(2);
    expect(videoState.videoWidth).toBeGreaterThan(0);

    const videoSrc = videoState.src;

    // CRITICAL: Video src must use the local htree server URL in Tauri
    expect(videoSrc).toMatch(/^http:\/\/(localhost|127\.0\.0\.1):21417\/htree\//);
    expect(videoSrc).not.toContain('blob:');
    expect(videoSrc).toContain('/htree/nhash1');

    // Test if htree server responds correctly to video request with Range header
    // Note: fetch() doesn't work with custom htree:// protocol, only with http://
    if (videoSrc.startsWith('http://')) {
      const httpTest = await browser.execute(async (videoUrl: string) => {
        try {
          const headResponse = await fetch(videoUrl, { method: 'HEAD' });
          let rangeStatus = null;
          if (headResponse.status === 200) {
            const rangeResponse = await fetch(videoUrl, {
              method: 'GET',
              headers: { Range: 'bytes=0-1024' },
            });
            rangeStatus = rangeResponse.status;
          }

          return {
            url: videoUrl,
            headStatus: headResponse.status,
            rangeStatus,
            error: null,
          };
        } catch (e) {
          return { error: String(e), url: videoUrl };
        }
      }, videoSrc);

      // Server should return 200 for the video
      expect(httpTest.error).toBeNull();
      expect(httpTest.headStatus).toBe(200);

      // Check if Range requests work (important for video streaming)
      if (httpTest.rangeStatus !== null) {
        expect(httpTest.rangeStatus).toBe(206);
      }
    }
  });

  it('should have mediaUrl functions using correct Tauri prefix', async () => {
    // Test that mediaUrl.ts functions work correctly in Tauri
    await switchToVideoWebview();
    await waitForHtreeApi();
    const urlTest = await browser.execute(() => {
      const htree = (window as any).htree;
      if (!htree) return { error: 'htree not available' };

      const baseUrl = htree.htreeBaseUrl;

      // Construct a test URL like getNpubFileUrl would
      const testNpub = 'npub1test123';
      const testTreeName = 'videos/Test Video';
      const testPath = 'video.mp4';

      const encodedTreeName = encodeURIComponent(testTreeName);
      const expectedUrl = `${baseUrl}/htree/${testNpub}/${encodedTreeName}/${testPath}`;

      return {
        baseUrl,
        expectedUrl,
        usesHttpPrefix: baseUrl.startsWith('http://'),
        usesCustomProtocol: baseUrl.startsWith('htree://'),
        hasPort: /:\d+$/.test(baseUrl),
      };
    });

    // Should use either HTTP localhost or custom htree:// protocol
    expect(urlTest.usesHttpPrefix || urlTest.usesCustomProtocol).toBe(true);
    // hasPort only applies to HTTP URLs
    if (urlTest.usesHttpPrefix) {
      expect(urlTest.hasPort).toBe(true);
    }
    if (urlTest.usesHttpPrefix) {
      expect(urlTest.baseUrl).toMatch(/^http:\/\/(localhost|127\.0\.0\.1):21417$/);
    }
    expect(urlTest.expectedUrl).toContain('/htree/npub1test123/videos%2FTest%20Video/video.mp4');
  });

  it('should detect local relay if running', async () => {
    // Test the detectLocalRelay function
    await switchToVideoWebview();
    await waitForHtreeApi();
    const relayCheck = await browser.execute(async () => {
      const htree = (window as any).htree;
      if (!htree || !htree.detectLocalRelay) {
        return { error: 'detectLocalRelay not available' };
      }

      try {
        const localRelay = await htree.detectLocalRelay();
        return {
          found: !!localRelay,
          url: localRelay,
        };
      } catch (e) {
        return {
          error: String(e),
        };
      }
    });

    // Function should work (doesn't matter if relay is found or not)
    expect(relayCheck.error).toBeUndefined();
  });

  it('thumbnail URLs should use Tauri htree prefix', async () => {
    // Check that thumbnails use correct URLs
    await switchToVideoWebview();
    await waitForHtreeApi();
    const thumbnailCheck = await browser.execute(() => {
      const htree = (window as any).htree;
      const images = document.querySelectorAll('img');
      const htreeThumbnails = [];

      for (const img of images) {
        const src = img.src || img.getAttribute('src') || '';
        if (src.includes('/htree/') || src.includes('htree://')) {
          htreeThumbnails.push({
            src,
            usesHttpPrefix: src.startsWith('http://127.0.0.1') || src.startsWith('http://localhost'),
            usesCustomProtocol: src.startsWith('htree://'),
          });
        }
      }

      return {
        baseUrl: htree?.htreeBaseUrl,
        thumbnailCount: htreeThumbnails.length,
        thumbnails: htreeThumbnails.slice(0, 5), // First 5
      };
    });

    // If there are htree thumbnails, they should use either http or htree:// prefix in Tauri
    if (thumbnailCheck.thumbnailCount > 0) {
      for (const thumb of thumbnailCheck.thumbnails) {
        expect(thumb.usesHttpPrefix || thumb.usesCustomProtocol).toBe(true);
      }
    }
  });

  it('should resolve /thumbnail for production video tree', async function () {
    this.timeout(120000);
    await switchToVideoWebview();
    await enableHtreeDebug();
    await forceVideoSettings();
    try {
      await browser.setWindowSize(1600, 1000);
    } catch {}
    await browser.execute((npub: string, treeName: string, debugQuery: string) => {
      window.location.hash = `#/${npub}/${treeName}?${debugQuery}`;
    }, TEST_PROD_NPUB, TEST_PROD_TREE_NAME_ENCODED, DEBUG_QUERY);
    await browser.pause(1500);
    await waitForHtreeApi();

    try {
      await browser.waitUntil(
        async () => {
          const state = await browser.execute(() => {
            const video = document.querySelector('video') as HTMLVideoElement | null;
            if (!video) {
              return { ready: false };
            }
            const src = video.currentSrc || video.src || '';
            return {
              ready: !!src && video.readyState >= 2 && video.videoWidth > 0,
            };
          });
          return state.ready;
        },
        {
          timeout: 45000,
          interval: 1000,
          timeoutMsg: 'Video element did not load media data for production tree',
        }
      );
    } catch (err) {
      const debugState = await browser.execute(() => {
        const video = document.querySelector('video') as HTMLVideoElement | null;
        const src = video?.currentSrc || video?.src || '';
        const errorText = document.querySelector('[class*="error"]')?.textContent || '';
        const videoContainer = document.querySelector('[data-video-src]');
        const htree = (window as any).htree;

        return {
          hasVideo: !!video,
          src,
          readyState: video?.readyState ?? null,
          videoWidth: video?.videoWidth ?? null,
          errorText,
          dataVideoSrc: videoContainer?.getAttribute('data-video-src') || '',
          dataVideoFilename: videoContainer?.getAttribute('data-video-filename') || '',
          dataHtreePrefix: videoContainer?.getAttribute('data-htree-prefix') || '',
          dataVideoLoadRuns: videoContainer?.getAttribute('data-video-load-runs') || '',
          dataVideoKey: videoContainer?.getAttribute('data-video-key') || '',
          dataVideoRootHash: videoContainer?.getAttribute('data-video-root-hash') || '',
          dataVideoNpub: videoContainer?.getAttribute('data-video-npub') || '',
          dataVideoTreeName: videoContainer?.getAttribute('data-video-tree-name') || '',
          location: {
            href: window.location.href,
            protocol: window.location.protocol,
            hash: window.location.hash,
          },
          htreeApi: htree
            ? {
                isTauri: htree.isTauri,
                baseUrl: htree.htreeBaseUrl,
                npub: htree.npub,
                isLoggedIn: htree.isLoggedIn,
              }
            : null,
          tauriGlobals: {
            hasTauri: '__TAURI__' in window,
            hasTauriInternals: '__TAURI_INTERNALS__' in window,
            serverOverride: (window as any).__HTREE_SERVER_URL__ || null,
          },
          storage: {
            debugFlag: localStorage.getItem('htree.debug'),
          },
          debugFlags: {
            windowFlag: (window as any).__HTREE_DEBUG__ ?? null,
            hash: window.location.hash,
          },
          debugLog: (window as any).__HTREE_DEBUG_LOG__?.slice(-80) || [],
          videoLog: ((window as any).__HTREE_DEBUG_LOG__ || []).filter((entry: any) =>
            typeof entry?.event === 'string' && entry.event.startsWith('video:')
          ).slice(-40),
          consoleLogs: (window as any).__consoleLogs?.slice(-40) || [],
        };
      });
      console.log(`[E2E] Production video debug: ${JSON.stringify(debugState)}`);
      await takeScreenshot('video-ui-not-ready');
      throw err;
    }

    const videoState = await browser.execute(() => {
      const video = document.querySelector('video') as HTMLVideoElement | null;
      return {
        src: video?.currentSrc || video?.src || '',
        readyState: video?.readyState ?? 0,
        videoWidth: video?.videoWidth ?? 0,
      };
    });

    expect(videoState.src).toMatch(/^http:\/\/(localhost|127\.0\.0\.1):21417\/htree\//);
    expect(videoState.src).not.toContain('blob:');
    expect(videoState.readyState).toBeGreaterThanOrEqual(2);
    expect(videoState.videoWidth).toBeGreaterThan(0);

    await takeScreenshot('video-ui-before-thumbs');

    const state = await browser.execute((npub: string, treeNameEncoded: string) => {
      const htree = (window as any).htree;
      const baseUrl = htree?.htreeBaseUrl || '';
      const treeName = decodeURIComponent(treeNameEncoded);
      const encodedTreeName = encodeURIComponent(treeName);
      return {
        baseUrl,
        thumbUrl: `${baseUrl}/htree/${npub}/${encodedTreeName}/thumbnail`,
      };
    }, TEST_PROD_NPUB, TEST_PROD_TREE_NAME_ENCODED);

    expect(state.baseUrl).toMatch(/^http:\/\/(localhost|127\.0\.0\.1):21417$/);
    expect(state.thumbUrl).toContain(`/htree/${TEST_PROD_NPUB}/`);
    expect(state.thumbUrl).toContain('/thumbnail');

    await browser.waitUntil(
      async () => {
        const result = await browser.execute(async (thumbUrl: string) => {
          try {
            const response = await fetch(thumbUrl);
            return {
              status: response.status,
              contentType: response.headers.get('content-type') || '',
            };
          } catch (e) {
            return { status: 0, contentType: '', error: String(e) };
          }
        }, state.thumbUrl);

        if ((result as any).error) return false;
        return result.status === 200 && result.contentType.startsWith('image/');
      },
      {
        timeout: 90000,
        interval: 2000,
        timeoutMsg: 'Thumbnail not available via /thumbnail route',
      }
    );

    try {
      await browser.waitUntil(
        async () => {
          const result = await browser.execute(() => {
            const images = Array.from(document.querySelectorAll('img')) as HTMLImageElement[];
            const thumbs = images.filter((img) => (img.currentSrc || img.src || '').includes('/thumbnail'));
            const loaded = thumbs.filter((img) => img.complete && img.naturalWidth > 0);
            return { total: thumbs.length, loaded: loaded.length };
          });
          return result.loaded > 0;
        },
        {
          timeout: 60000,
          interval: 1000,
          timeoutMsg: 'No loaded thumbnail images found in the UI',
        }
      );
    } catch (err) {
      const thumbDebug = await browser.execute(() => {
        const images = Array.from(document.querySelectorAll('img')) as HTMLImageElement[];
        const thumbs = images
          .map((img) => ({
            src: img.currentSrc || img.src || '',
            complete: img.complete,
            naturalWidth: img.naturalWidth,
          }))
          .filter((img) => img.src.includes('/thumbnail'));
        const feedLinks = Array.from(document.querySelectorAll('a'))
          .map((link) => link.getAttribute('href') || '')
          .filter((href) => href.includes('/video') || href.includes('/videos') || href.includes('#/'));
        return {
          total: thumbs.length,
          samples: thumbs.slice(0, 5),
          feedLinkCount: feedLinks.length,
          debugLog: (window as any).__HTREE_DEBUG_LOG__?.slice(-30) || [],
        };
      });
      console.log(`[E2E] Thumbnail debug: ${JSON.stringify(thumbDebug)}`);
      await takeScreenshot('video-ui-thumbs-missing');
      throw err;
    }

    await takeScreenshot('video-ui-loaded');
  });
});
