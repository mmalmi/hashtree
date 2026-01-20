/**
 * Re-export hashtree utilities for tests.
 * This module exists so tests can import through Vite's bundler
 * instead of directly from node_modules (which has unresolved msgpack deps).
 */
export { nhashEncode, nhashDecode, toHex, fromHex, videoChunker, cid, LinkType, BlossomStore } from 'hashtree';
