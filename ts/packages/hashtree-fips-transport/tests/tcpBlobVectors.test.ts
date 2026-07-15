import { describe, expect, it } from 'vitest';
import {
  TCP_BLOB_DEFAULT_HTL,
  TCP_BLOB_MAX_BYTES,
  TCP_BLOB_MAX_HTL,
  TCP_BLOB_SERVICE_PORT,
  decodeTcpBlobRequest,
  decodeTcpBlobResponseHeader,
  encodeTcpBlobRequest,
  encodeTcpBlobResponseHeader,
} from '../src/tcpBlobTransport.js';

describe('TCP/FIPS Hashtree blob v1 vectors', () => {
  it('matches the Rust request and response-header encoding', () => {
    const hash = Uint8Array.from({ length: 32 }, (_, index) => index);
    expect(toHex(encodeTcpBlobRequest(hash))).toBe(
      `4801010a${Array.from(hash, (byte) => byte.toString(16).padStart(2, '0')).join('')}`,
    );
    expect(toHex(encodeTcpBlobResponseHeader(true, 3))).toBe('48010100000003');
    expect(TCP_BLOB_SERVICE_PORT).toBe(39_018);
    expect(TCP_BLOB_MAX_BYTES).toBe(16 * 1024 * 1024);
    expect(TCP_BLOB_DEFAULT_HTL).toBe(10);
    expect(TCP_BLOB_MAX_HTL).toBe(10);
  });

  it('round-trips the native HTL boundaries', () => {
    const hash = Uint8Array.from({ length: 32 }, (_, index) => index);
    expect(decodeTcpBlobRequest(encodeTcpBlobRequest(hash, 0))).toEqual({ htl: 0, hash });
    const request = encodeTcpBlobRequest(hash, TCP_BLOB_MAX_HTL);
    expect(toHex(request)).toBe(
      `4801010a${Array.from(hash, (byte) => byte.toString(16).padStart(2, '0')).join('')}`,
    );
    expect(decodeTcpBlobRequest(request)).toEqual({ htl: TCP_BLOB_MAX_HTL, hash });
  });

  it('rejects legacy request framing and invalid HTLs', () => {
    const hash = new Uint8Array(32);
    expect(() => decodeTcpBlobRequest(fromHex(`480101${'00'.repeat(32)}`))).toThrow(
      'invalid TCP/FIPS blob request',
    );
    for (const htl of [-1, 0.5, 11, 0xff, 256]) {
      expect(() => encodeTcpBlobRequest(hash, htl)).toThrow('TCP/FIPS blob HTL is invalid');
    }
    for (const htl of [11, 0xff]) {
      const request = encodeTcpBlobRequest(hash, 0);
      request[3] = htl;
      expect(() => decodeTcpBlobRequest(request)).toThrow('TCP/FIPS blob HTL is invalid');
    }
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
