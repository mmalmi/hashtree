import { describe, expect, it } from 'vitest';
import {
  TCP_BLOB_MAX_BYTES,
  TCP_BLOB_SERVICE_PORT,
  decodeTcpBlobResponseHeader,
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

  it('accepts only canonical missing and bounded found response headers', () => {
    expect(decodeTcpBlobResponseHeader(fromHex('48010000000000'))).toEqual({
      found: false,
      size: 0,
    });
    expect(decodeTcpBlobResponseHeader(fromHex('48010100000003'))).toEqual({
      found: true,
      size: 3,
    });
  });

  it('rejects unknown response statuses and non-canonical missing responses', () => {
    expect(() => decodeTcpBlobResponseHeader(fromHex('48010200000000'))).toThrow(
      'unsupported TCP/FIPS blob response status',
    );
    expect(() => decodeTcpBlobResponseHeader(fromHex('48010000000001'))).toThrow(
      'missing TCP/FIPS blob response has non-zero size',
    );
    expect(() => encodeTcpBlobResponseHeader(false, 1)).toThrow(
      'missing TCP/FIPS blob response has non-zero size',
    );
  });

  it('rejects found responses above the protocol size limit', () => {
    const header = fromHex('48010100000000');
    new DataView(header.buffer, header.byteOffset + 3, 4).setUint32(0, TCP_BLOB_MAX_BYTES + 1);
    expect(() => decodeTcpBlobResponseHeader(header)).toThrow(
      'TCP/FIPS blob exceeds size limit',
    );
  });
});

function toHex(bytes: Uint8Array): string {
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('');
}

function fromHex(hex: string): Uint8Array {
  return Uint8Array.from(hex.match(/../g) ?? [], (byte) => Number.parseInt(byte, 16));
}
