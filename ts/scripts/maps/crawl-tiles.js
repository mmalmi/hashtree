#!/usr/bin/env node
import {
  HashTree,
  BlossomStore,
  LinkType,
  cid,
  nhashEncode,
  toHex,
  createNostrRefResolver,
} from '../../packages/hashtree/dist/index.js';
import { finalizeEvent, getPublicKey, nip19 } from 'nostr-tools';
import { SimplePool, useWebSocketImplementation } from 'nostr-tools/pool';
import WebSocket from 'ws';
import { webcrypto } from 'node:crypto';
import { promises as fs } from 'node:fs';
import path from 'node:path';

const DEFAULT_TILE_URL = 'https://tile.openstreetmap.org/{z}/{x}/{y}.png';
const DEFAULT_STORE_DIR = '.maps-store';
const DEFAULT_CONCURRENCY = 8;
const DEFAULT_RETRIES = 2;
const DEFAULT_RETRY_DELAY_MS = 1000;
const DEFAULT_PROGRESS_INTERVAL_MS = 1000;
const DEFAULT_AVG_BYTES = 25_000;

function ensureWebCrypto() {
  if (!globalThis.crypto?.subtle) {
    globalThis.crypto = webcrypto;
  }
}

function getArg(args, name) {
  const idx = args.indexOf(name);
  if (idx === -1) return null;
  return args[idx + 1] ?? null;
}

function hasFlag(args, name) {
  return args.includes(name);
}

function parseCsv(value) {
  if (!value) return [];
  return value.split(',').map(entry => entry.trim()).filter(Boolean);
}

function parseBounds(value) {
  if (!value) return null;
  const parts = value.split(',').map(Number);
  if (parts.length !== 4 || parts.some(Number.isNaN)) return null;
  const [south, west, north, east] = parts;
  return { south, west, north, east };
}

function clampLat(lat) {
  const maxLat = 85.05112878;
  return Math.max(-maxLat, Math.min(maxLat, lat));
}

function clampLon(lon) {
  const wrapped = ((lon + 180) % 360 + 360) % 360 - 180;
  return Number.isFinite(wrapped) ? wrapped : lon;
}

function lonToTileX(lon, zoom) {
  const n = 2 ** zoom;
  return Math.floor((clampLon(lon) + 180) / 360 * n);
}

function latToTileY(lat, zoom) {
  const n = 2 ** zoom;
  const rad = (clampLat(lat) * Math.PI) / 180;
  return Math.floor(
    (1 - Math.log(Math.tan(rad) + 1 / Math.cos(rad)) / Math.PI) / 2 * n
  );
}

function getTileRange(bounds, zoom) {
  const minX = lonToTileX(bounds.west, zoom);
  const maxX = lonToTileX(bounds.east, zoom);
  const minY = latToTileY(bounds.north, zoom);
  const maxY = latToTileY(bounds.south, zoom);
  return { minX, maxX, minY, maxY };
}

function estimateTileCounts(bounds, minZoom, maxZoom) {
  const perZoom = [];
  let total = 0;
  for (let zoom = minZoom; zoom <= maxZoom; zoom += 1) {
    const { minX, maxX, minY, maxY } = getTileRange(bounds, zoom);
    const count = Math.max(0, maxX - minX + 1) * Math.max(0, maxY - minY + 1);
    perZoom.push({ zoom, tiles: count, minX, maxX, minY, maxY });
    total += count;
  }
  return { total, perZoom };
}

function formatBytes(bytes) {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let value = bytes;
  let idx = 0;
  while (value >= 1024 && idx < units.length - 1) {
    value /= 1024;
    idx += 1;
  }
  return `${value.toFixed(value >= 10 || idx === 0 ? 0 : 1)} ${units[idx]}`;
}

function formatDuration(seconds) {
  if (!Number.isFinite(seconds) || seconds <= 0) return '0s';
  const hrs = Math.floor(seconds / 3600);
  const mins = Math.floor((seconds % 3600) / 60);
  const secs = Math.floor(seconds % 60);
  const parts = [];
  if (hrs) parts.push(`${hrs}h`);
  if (mins) parts.push(`${mins}m`);
  parts.push(`${secs}s`);
  return parts.join(' ');
}

function stableStringify(value) {
  if (value === null || typeof value !== 'object') {
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) {
    return `[${value.map(item => stableStringify(item)).join(',')}]`;
  }
  const keys = Object.keys(value).sort();
  const entries = keys.map(key => `${JSON.stringify(key)}:${stableStringify(value[key])}`);
  return `{${entries.join(',')}}`;
}

function buildTileUrl(template, z, x, y) {
  return template
    .replace('{z}', z.toString())
    .replace('{x}', x.toString())
    .replace('{y}', y.toString());
}

function guessExtension(template) {
  const match = template.match(/\.([a-z0-9]+)(?:\?|$)/i);
  return match ? match[1].toLowerCase() : 'png';
}

class FileStore {
  constructor(rootDir) {
    this.rootDir = rootDir;
  }

  pathFor(hash) {
    const hex = toHex(hash);
    return path.join(this.rootDir, hex.slice(0, 2), hex.slice(2, 4), `${hex}.bin`);
  }

  async put(hash, data) {
    const filePath = this.pathFor(hash);
    try {
      await fs.access(filePath);
      return false;
    } catch (err) {
      if (err?.code !== 'ENOENT') throw err;
    }
    await fs.mkdir(path.dirname(filePath), { recursive: true });
    await fs.writeFile(filePath, data);
    return true;
  }

  async get(hash) {
    const filePath = this.pathFor(hash);
    try {
      const data = await fs.readFile(filePath);
      return new Uint8Array(data);
    } catch (err) {
      if (err?.code === 'ENOENT') return null;
      throw err;
    }
  }

  async has(hash) {
    const filePath = this.pathFor(hash);
    try {
      await fs.access(filePath);
      return true;
    } catch {
      return false;
    }
  }

  async delete(hash) {
    const filePath = this.pathFor(hash);
    try {
      await fs.unlink(filePath);
      return true;
    } catch (err) {
      if (err?.code === 'ENOENT') return false;
      throw err;
    }
  }
}

async function pushToBlossom(tree, rootCid, servers, nsec) {
  if (!servers.length) return { pushed: 0, skipped: 0, failed: 0 };
  if (!nsec) {
    throw new Error('nsec required for Blossom uploads');
  }
  const decoded = nip19.decode(nsec);
  if (decoded.type !== 'nsec') {
    throw new Error('Invalid nsec');
  }
  const secretKey = decoded.data;
  const signer = async (event) => finalizeEvent(event, secretKey);
  const blossom = new BlossomStore({
    servers: servers.map(url => ({ url, read: true, write: true })),
    signer,
  });
  const result = await tree.push(rootCid, blossom, {
    onProgress: (current, total) => {
      process.stdout.write(`Blossom push: ${current}/${total}\r`);
    },
  });
  process.stdout.write('\n');
  return { pushed: result.pushed, skipped: result.skipped, failed: result.failed };
}

async function publishRoot({ rootCid, treeName, relays, nsec }) {
  if (!nsec) {
    throw new Error('nsec required for Nostr publishing');
  }
  if (!relays?.length) {
    throw new Error('At least one relay required for Nostr publishing');
  }
  useWebSocketImplementation(WebSocket);
  const decoded = nip19.decode(nsec);
  if (decoded.type !== 'nsec') {
    throw new Error('Invalid nsec');
  }
  const secretKey = decoded.data;
  const pubkey = getPublicKey(secretKey);
  const npub = nip19.npubEncode(pubkey);
  const pool = new SimplePool({ enablePing: true });

  const resolver = createNostrRefResolver({
    subscribe: () => () => {},
    publish: async (event) => {
      const template = {
        ...event,
        created_at: event.created_at ?? Math.floor(Date.now() / 1000),
        pubkey,
      };
      const signed = finalizeEvent(template, secretKey);
      const pubs = pool.publish(relays, signed);
      await Promise.any(pubs);
      return true;
    },
    getPubkey: () => pubkey,
    nip19,
  });

  const key = `${npub}/${treeName}`;
  const result = await resolver.publish(key, cid(rootCid.hash, rootCid.key), { visibility: 'public' });

  pool.close(relays);
  return { npub, result, nhash: nhashEncode(rootCid) };
}

async function main() {
  ensureWebCrypto();
  const args = process.argv.slice(2);
  const boundsValue = getArg(args, '--bbox') || getArg(args, '--bounds');
  const bounds = parseBounds(boundsValue);
  const minZoom = Number(getArg(args, '--min-zoom') || '0');
  const maxZoom = Number(getArg(args, '--max-zoom') || '0');
  const template = getArg(args, '--url') || DEFAULT_TILE_URL;
  const ext = (getArg(args, '--ext') || guessExtension(template)).toLowerCase();
  const concurrency = Number(getArg(args, '--concurrency') || DEFAULT_CONCURRENCY);
  const retries = Number(getArg(args, '--retries') || DEFAULT_RETRIES);
  const retryDelayMs = Number(getArg(args, '--retry-delay') || DEFAULT_RETRY_DELAY_MS);
  const progressIntervalMs = Number(getArg(args, '--progress-interval') || DEFAULT_PROGRESS_INTERVAL_MS);
  const avgBytesOverride = Number(getArg(args, '--avg-bytes') || DEFAULT_AVG_BYTES);
  const storeDir = getArg(args, '--store-dir') || DEFAULT_STORE_DIR;
  const treeName = getArg(args, '--tree') || 'maps';
  const userAgent = getArg(args, '--user-agent') || 'IrisMapsCrawler/1.0';
  const shouldPublish = hasFlag(args, '--publish');
  const estimateOnly = hasFlag(args, '--estimate-only') || hasFlag(args, '--estimate');
  const blossomServers = parseCsv(getArg(args, '--blossom') || process.env.BLOSSOM_SERVERS);
  const relays = parseCsv(getArg(args, '--relays') || process.env.NOSTR_RELAYS);
  const nsec = getArg(args, '--nsec') || process.env.NOSTR_NSEC;

  if (!bounds) {
    console.error('Provide --bbox south,west,north,east');
    process.exit(1);
  }
  if (bounds.west > bounds.east) {
    console.error('Bounds crossing dateline not supported');
    process.exit(1);
  }
  if (Number.isNaN(minZoom) || Number.isNaN(maxZoom) || maxZoom < minZoom) {
    console.error('Provide --min-zoom and --max-zoom');
    process.exit(1);
  }
  if (!Number.isFinite(concurrency) || concurrency <= 0) {
    console.error('Provide --concurrency > 0');
    process.exit(1);
  }

  const counts = estimateTileCounts(bounds, minZoom, maxZoom);
  const estimatedBytes = counts.total * avgBytesOverride;

  if (estimateOnly) {
    console.log(`Tiles: ${counts.total}`);
    console.log(`Estimated size (@${avgBytesOverride} bytes/tile): ${formatBytes(estimatedBytes)}`);
    process.exit(0);
  }

  const store = new FileStore(storeDir);
  const tree = new HashTree({ store });
  const { cid: emptyRoot } = await tree.putDirectory([], { unencrypted: true });
  let rootCid = emptyRoot;
  let persistChain = Promise.resolve();

  const queueRootUpdate = (action) => {
    persistChain = persistChain.then(async () => {
      rootCid = await action(rootCid);
    });
    return persistChain;
  };

  const ensureDirs = async (currentRoot, parts) => {
    if (parts.length === 0) return currentRoot;
    let nextRoot = currentRoot;
    const pathParts = [];
    for (const segment of parts) {
      pathParts.push(segment);
      const existing = await tree.resolvePath(nextRoot, pathParts);
      if (existing?.type === LinkType.Dir) {
        continue;
      }
      const { cid: dirCid } = await tree.putDirectory([], { unencrypted: true });
      nextRoot = await tree.setEntry(nextRoot, pathParts.slice(0, -1), segment, dirCid, 0, LinkType.Dir);
    }
    return nextRoot;
  };

  const meta = {
    version: 1,
    dataset: {
      type: 'tiles',
      urlTemplate: template,
      bounds: [bounds.south, bounds.west, bounds.north, bounds.east],
      minZoom,
      maxZoom,
      ext,
      scheme: 'web-mercator',
      tileSize: 256,
    },
    counts: {
      totalTiles: counts.total,
      perZoom: counts.perZoom.map(({ zoom, tiles }) => ({ zoom, tiles })),
    },
  };

  const metaBytes = new TextEncoder().encode(stableStringify(meta));
  await queueRootUpdate(async (currentRoot) => {
    const { cid: fileCid, size } = await tree.putFile(metaBytes, { unencrypted: true });
    return tree.setEntry(currentRoot, [], 'meta.json', fileCid, size, LinkType.File);
  });

  await queueRootUpdate(async (currentRoot) => ensureDirs(currentRoot, ['tiles']));

  const startTime = Date.now();
  let processed = 0;
  let succeeded = 0;
  let failed = 0;
  let skipped = 0;
  let downloadedBytes = 0;
  let lastLog = 0;

  const logProgress = (force = false) => {
    const now = Date.now();
    if (!force && now - lastLog < progressIntervalMs) return;
    lastLog = now;
    const elapsedSec = (now - startTime) / 1000;
    const rate = processed > 0 ? processed / elapsedSec : 0;
    const percent = counts.total > 0 ? (processed / counts.total) * 100 : 0;
    const avgBytes = succeeded > 0 ? downloadedBytes / succeeded : avgBytesOverride;
    const estimateTotal = avgBytes * counts.total;
    const remainingTiles = Math.max(0, counts.total - processed);
    const etaSec = rate > 0 ? remainingTiles / rate : 0;
    const remainingBytes = Math.max(0, estimateTotal - downloadedBytes);
    console.log(
      `Tiles ${processed}/${counts.total} (${percent.toFixed(1)}%) ` +
      `ok=${succeeded} skip=${skipped} fail=${failed} ` +
      `size=${formatBytes(downloadedBytes)} ` +
      `est=${formatBytes(estimateTotal)} remaining=${formatBytes(remainingBytes)} ` +
      `rate=${rate.toFixed(2)}/s eta=${formatDuration(etaSec)}`
    );
  };

  const fetchTile = async (url) => {
    for (let attempt = 0; attempt <= retries; attempt += 1) {
      try {
        const response = await fetch(url, {
          headers: { 'User-Agent': userAgent },
          signal: AbortSignal.timeout(30000),
        });
        if (!response.ok) {
          return { ok: false, status: response.status };
        }
        const buffer = await response.arrayBuffer();
        return { ok: true, data: new Uint8Array(buffer) };
      } catch (err) {
        if (attempt >= retries) {
          return { ok: false, error: err };
        }
        await new Promise(resolve => setTimeout(resolve, retryDelayMs));
      }
    }
    return { ok: false, error: new Error('Unknown fetch error') };
  };

  const tasks = new Set();

  for (const zoomInfo of counts.perZoom) {
    for (let x = zoomInfo.minX; x <= zoomInfo.maxX; x += 1) {
      for (let y = zoomInfo.minY; y <= zoomInfo.maxY; y += 1) {
        while (tasks.size >= concurrency) {
          await Promise.race(tasks);
        }

        const task = (async () => {
          const url = buildTileUrl(template, zoomInfo.zoom, x, y);
          const result = await fetchTile(url);
          processed += 1;

          if (!result.ok || !result.data || result.data.length === 0) {
            if (result.status === 404) {
              skipped += 1;
            } else {
              failed += 1;
            }
            logProgress();
            return;
          }

          const tilePath = ['tiles', zoomInfo.zoom.toString(), x.toString()];
          const tileName = `${y}.${ext}`;
          const tileData = result.data;
          downloadedBytes += tileData.length;
          succeeded += 1;

          await queueRootUpdate(async (currentRoot) => {
            let nextRoot = await ensureDirs(currentRoot, tilePath);
            const { cid: fileCid, size } = await tree.putFile(tileData, { unencrypted: true });
            nextRoot = await tree.setEntry(nextRoot, tilePath, tileName, fileCid, size, LinkType.File);
            return nextRoot;
          });

          logProgress();
        })();

        tasks.add(task);
        task.finally(() => tasks.delete(task));
      }
    }
  }

  await Promise.all(tasks);
  await persistChain;
  logProgress(true);

  console.log(`Root hash: ${toHex(rootCid.hash)}`);
  console.log(`nhash: ${nhashEncode(rootCid)}`);
  console.log(`Tiles stored: ${succeeded}/${counts.total}`);
  console.log(`Stored bytes: ${formatBytes(downloadedBytes)}`);

  if (blossomServers.length > 0) {
    const blossomResult = await pushToBlossom(tree, rootCid, blossomServers, nsec);
    console.log(`Blossom: pushed=${blossomResult.pushed} skipped=${blossomResult.skipped} failed=${blossomResult.failed}`);
  }

  if (shouldPublish) {
    const published = await publishRoot({ rootCid, treeName, relays, nsec });
    console.log(`Published: ${published.result.success ? 'yes' : 'no'} npub=${published.npub}`);
    console.log(`nhash=${published.nhash}`);
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
