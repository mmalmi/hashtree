import { describe, expect, it } from 'vitest';
import {
  TCP_BLOB_MAX_BYTES,
  TCP_BLOB_SERVICE_PORT,
  encodeTcpBlobRequest,
  encodeTcpBlobResponseHeader,
} from '../src/tcpBlobTransport.js';

describe('TCP/FIPS Hashtree blob v1 vectors', () => {
  it('matches the Rust request and response-header encoding', () => {
    const hash = Uint8Array.from({ length: 32 }, (_, index) => index);
    expect(toHex(encodeTcpBlobRequest(hash))).toBe(
      `480101${Array.from(hash, (byte) => byte.toString(16).padStart(2, '0')).join('')}`,
    );
    expect(toHex(encodeTcpBlobResponseHeader(true, 3))).toBe('48010100000003');
    expect(TCP_BLOB_SERVICE_PORT).toBe(39_018);
    expect(TCP_BLOB_MAX_BYTES).toBe(16 * 1024 * 1024);
  });
});

function toHex(bytes: Uint8Array): string {
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('');
}
