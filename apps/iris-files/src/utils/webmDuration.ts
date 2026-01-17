/**
 * WebM Duration Patcher
 *
 * WebM files created by MediaRecorder typically have unknown duration.
 * This utility finds and patches the Duration element in the WebM header.
 *
 * WebM uses EBML format where Duration is stored as:
 * - Element ID: 0x4489
 * - Size: variable length (usually 4 bytes for float)
 * - Value: float (duration in milliseconds * timecode scale)
 */

import type { CID, HashTree } from 'hashtree';

/**
 * Find the byte offset of the Duration element in a WebM file
 * Returns { offset, size } where offset is where to write and size is the value length
 */
function findDurationOffset(data: Uint8Array): { offset: number; size: number } | null {
  let i = 0;

  // Helper to read variable-size EBML size
  function readEbmlSize(pos: number): { size: number; len: number } | null {
    if (pos >= data.length) return null;
    const first = data[pos];
    if (first & 0x80) return { size: first & 0x7f, len: 1 };
    if (first & 0x40) return { size: ((first & 0x3f) << 8) | data[pos + 1], len: 2 };
    if (first & 0x20) return { size: ((first & 0x1f) << 16) | (data[pos + 1] << 8) | data[pos + 2], len: 3 };
    if (first & 0x10) return { size: ((first & 0x0f) << 24) | (data[pos + 1] << 16) | (data[pos + 2] << 8) | data[pos + 3], len: 4 };
    // Unknown size (all 1s) - common for streaming
    if (first === 0x01 && data[pos + 1] === 0xff && data[pos + 2] === 0xff && data[pos + 3] === 0xff &&
        data[pos + 4] === 0xff && data[pos + 5] === 0xff && data[pos + 6] === 0xff && data[pos + 7] === 0xff) {
      return { size: -1, len: 8 }; // Unknown size marker
    }
    return null;
  }

  // Search for Duration element (0x4489) in first 2KB
  const searchLimit = Math.min(data.length, 2048);

  while (i < searchLimit) {
    // Check for Duration element ID (0x4489 = 2 bytes: 0x44, 0x89)
    if (data[i] === 0x44 && data[i + 1] === 0x89) {
      const sizeInfo = readEbmlSize(i + 2);
      if (sizeInfo) {
        // Duration value starts after ID (2 bytes) + size field
        const valueOffset = i + 2 + sizeInfo.len;
        return { offset: valueOffset, size: sizeInfo.size };
      }
    }
    i++;
  }

  return null;
}

/**
 * Encode a float64 as 8 bytes (big-endian IEEE 754)
 */
function encodeFloat64(value: number): Uint8Array {
  const buffer = new ArrayBuffer(8);
  const view = new DataView(buffer);
  view.setFloat64(0, value, false); // big-endian
  return new Uint8Array(buffer);
}

/**
 * Encode a float32 as 4 bytes (big-endian IEEE 754)
 */
function encodeFloat32(value: number): Uint8Array {
  const buffer = new ArrayBuffer(4);
  const view = new DataView(buffer);
  view.setFloat32(0, value, false); // big-endian
  return new Uint8Array(buffer);
}

/**
 * Patch the duration in a WebM file
 *
 * @param tree - HashTree instance
 * @param fileCid - CID of the WebM file
 * @param durationMs - Duration in milliseconds
 * @returns New CID with patched duration, or original CID if patching failed
 */
export async function patchWebmDuration(
  tree: HashTree,
  fileCid: CID,
  durationMs: number
): Promise<CID> {
  try {
    // Read the first 2KB to find the duration offset
    const header = await tree.readFileRange(fileCid, 0, 2048);
    if (!header) {
      console.warn('[WebM] Could not read file header');
      return fileCid;
    }

    const durationInfo = findDurationOffset(header);
    if (!durationInfo) {
      console.warn('[WebM] Duration element not found in header');
      return fileCid;
    }

    // WebM stores duration in nanoseconds (timecode scale is usually 1000000 = 1ms)
    // But the Duration element is stored as float relative to timecode scale
    // Default timecode scale is 1000000ns = 1ms, so duration is in ms
    const durationValue = durationMs;

    // Encode duration as float (4 bytes for float32, 8 bytes for float64)
    const durationBytes = durationInfo.size === 8
      ? encodeFloat64(durationValue)
      : encodeFloat32(durationValue);

    console.log(`[WebM] Patching duration at offset ${durationInfo.offset}, size=${durationInfo.size}, value=${durationMs}ms`);

    // Use writeAt to patch just the duration bytes
    const newCid = await tree.writeAt(fileCid, durationInfo.offset, durationBytes);

    console.log('[WebM] Duration patched successfully');
    return newCid;
  } catch (e) {
    console.error('[WebM] Failed to patch duration:', e);
    return fileCid;
  }
}
