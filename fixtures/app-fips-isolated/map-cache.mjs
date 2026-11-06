import { pathToFileURL } from 'node:url';
// Existing app cache verifies every retained byte. Network namespace blocks any
// missing-block retrieval; only the exact preverified public cache can succeed.
const { startMapAssetCache } = await import(pathToFileURL(process.argv[2] + '/scripts/portable-map-assets.mjs'));
const cache = await startMapAssetCache(process.argv[3]);
console.log(JSON.stringify({ url: cache.url }));
process.once('SIGTERM', async () => { await cache.close(); process.exit(0); });
