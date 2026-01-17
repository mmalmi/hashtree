/**
 * Protocol wire format tests for hashtree-ts <-> hashtree-rs interop
 *
 * These tests verify the binary MessagePack encoding is compatible between
 * TypeScript and Rust implementations.
 *
 * Wire format: [type byte][msgpack body]
 * Request:  [0x00][msgpack: {h: bytes32, htl?: u8}]
 * Response: [0x01][msgpack: {h: bytes32, d: bytes, i?: u32, n?: u32}]
 */

import { describe, it, expect } from 'vitest';
import {
  encodeRequest,
  encodeResponse,
  parseMessage,
  createRequest,
  createResponse,
  createFragmentResponse,
  isFragmented,
  hashToKey,
} from '../src/webrtc/protocol.js';
import {
  MSG_TYPE_REQUEST,
  MSG_TYPE_RESPONSE,
  FRAGMENT_SIZE,
} from '../src/webrtc/types.js';

// Helper to create test hash (32 bytes)
function testHash(pattern: number): Uint8Array {
  return new Uint8Array(32).fill(pattern);
}

// Convert ArrayBuffer to hex for comparison
function toHex(buf: ArrayBuffer | Uint8Array): string {
  const bytes = buf instanceof ArrayBuffer ? new Uint8Array(buf) : buf;
  return Array.from(bytes)
    .map((b) => b.toString(16).padStart(2, '0'))
    .join('');
}

describe('Protocol Wire Format', () => {
  describe('Request Encoding', () => {
    it('should encode request with type byte prefix', () => {
      const hash = testHash(0xab);
      const req = createRequest(hash, 10);
      const encoded = encodeRequest(req);
      const bytes = new Uint8Array(encoded);

      // First byte should be request type marker
      expect(bytes[0]).toBe(MSG_TYPE_REQUEST);
      expect(bytes[0]).toBe(0x00);
    });

    it('should round-trip request through encode/parse', () => {
      const hash = testHash(0xcd);
      const req = createRequest(hash, 7);
      const encoded = encodeRequest(req);

      const parsed = parseMessage(encoded);
      expect(parsed).not.toBeNull();
      expect(parsed!.type).toBe(MSG_TYPE_REQUEST);

      const body = parsed!.body as { h: Uint8Array; htl?: number };
      expect(toHex(body.h)).toBe(toHex(hash));
      expect(body.htl).toBe(7);
    });

    it('should handle request without htl', () => {
      const hash = testHash(0xef);
      const req = { h: hash }; // No htl
      const encoded = encodeRequest(req);

      const parsed = parseMessage(encoded);
      expect(parsed).not.toBeNull();

      const body = parsed!.body as { h: Uint8Array; htl?: number };
      expect(toHex(body.h)).toBe(toHex(hash));
      expect(body.htl).toBeUndefined();
    });
  });

  describe('Response Encoding', () => {
    it('should encode response with type byte prefix', () => {
      const hash = testHash(0x11);
      const data = new Uint8Array([1, 2, 3, 4, 5]);
      const res = createResponse(hash, data);
      const encoded = encodeResponse(res);
      const bytes = new Uint8Array(encoded);

      // First byte should be response type marker
      expect(bytes[0]).toBe(MSG_TYPE_RESPONSE);
      expect(bytes[0]).toBe(0x01);
    });

    it('should round-trip response through encode/parse', () => {
      const hash = testHash(0x22);
      const data = new Uint8Array([10, 20, 30, 40, 50]);
      const res = createResponse(hash, data);
      const encoded = encodeResponse(res);

      const parsed = parseMessage(encoded);
      expect(parsed).not.toBeNull();
      expect(parsed!.type).toBe(MSG_TYPE_RESPONSE);

      const body = parsed!.body as { h: Uint8Array; d: Uint8Array };
      expect(toHex(body.h)).toBe(toHex(hash));
      expect(toHex(body.d)).toBe(toHex(data));
    });

    it('should encode unfragmented response without i/n fields', () => {
      const hash = testHash(0x33);
      const data = new Uint8Array([1, 2, 3]);
      const res = createResponse(hash, data);

      expect(isFragmented(res)).toBe(false);
      expect(res.i).toBeUndefined();
      expect(res.n).toBeUndefined();
    });

    it('should encode fragmented response with i/n fields', () => {
      const hash = testHash(0x44);
      const data = new Uint8Array([1, 2, 3]);
      const res = createFragmentResponse(hash, data, 2, 5);

      expect(isFragmented(res)).toBe(true);
      expect(res.i).toBe(2);
      expect(res.n).toBe(5);

      // Round-trip test
      const encoded = encodeResponse(res);
      const parsed = parseMessage(encoded);
      const body = parsed!.body as {
        h: Uint8Array;
        d: Uint8Array;
        i?: number;
        n?: number;
      };
      expect(body.i).toBe(2);
      expect(body.n).toBe(5);
    });
  });

  describe('Parse Invalid Messages', () => {
    it('should return null for empty data', () => {
      expect(parseMessage(new ArrayBuffer(0))).toBeNull();
    });

    it('should return null for single byte', () => {
      expect(parseMessage(new Uint8Array([0x00]))).toBeNull();
    });

    it('should return null for invalid type', () => {
      expect(parseMessage(new Uint8Array([0xff, 0x00]))).toBeNull();
    });

    it('should return null for invalid msgpack', () => {
      // Type byte + garbage that's not valid msgpack
      expect(parseMessage(new Uint8Array([0x00, 0xff, 0xff, 0xff]))).toBeNull();
    });
  });

  describe('Hash Utilities', () => {
    it('should convert hash to hex key correctly', () => {
      const hash = new Uint8Array([
        0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01,
        0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89,
        0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89,
      ]);
      const key = hashToKey(hash);
      expect(key).toBe(
        'abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789'
      );
    });
  });

  describe('Constants', () => {
    it('should have correct type markers', () => {
      expect(MSG_TYPE_REQUEST).toBe(0x00);
      expect(MSG_TYPE_RESPONSE).toBe(0x01);
    });

    it('should have correct fragment size', () => {
      expect(FRAGMENT_SIZE).toBe(32 * 1024);
    });
  });

  describe('Interop Test Vectors', () => {
    // These vectors can be used to verify compatibility with hashtree-rs
    // Run `cargo test -p hashtree-webrtc -- --nocapture test_encode_decode` to compare

    it('should produce expected request encoding', () => {
      // Known test case: hash of all 0xab bytes, htl=10
      const hash = testHash(0xab);
      const req = createRequest(hash, 10);
      const encoded = encodeRequest(req);
      const bytes = new Uint8Array(encoded);

      // Verify structure
      expect(bytes[0]).toBe(0x00); // Request type
      // The rest is msgpack-encoded {h: bytes32, htl: 10}

      // Round-trip verify
      const parsed = parseMessage(encoded);
      expect(parsed!.type).toBe(MSG_TYPE_REQUEST);
      const body = parsed!.body as { h: Uint8Array; htl: number };
      expect(body.htl).toBe(10);
      expect(body.h.length).toBe(32);
      expect(body.h[0]).toBe(0xab);
    });

    it('should produce expected response encoding', () => {
      // Known test case: hash of all 0xcd bytes, data [1,2,3,4,5]
      const hash = testHash(0xcd);
      const data = new Uint8Array([1, 2, 3, 4, 5]);
      const res = createResponse(hash, data);
      const encoded = encodeResponse(res);
      const bytes = new Uint8Array(encoded);

      // Verify structure
      expect(bytes[0]).toBe(0x01); // Response type
      // The rest is msgpack-encoded {h: bytes32, d: [1,2,3,4,5]}

      // Round-trip verify
      const parsed = parseMessage(encoded);
      expect(parsed!.type).toBe(MSG_TYPE_RESPONSE);
      const body = parsed!.body as { h: Uint8Array; d: Uint8Array };
      expect(body.h.length).toBe(32);
      expect(body.h[0]).toBe(0xcd);
      expect(Array.from(body.d)).toEqual([1, 2, 3, 4, 5]);
    });

    it('should produce expected fragment response encoding', () => {
      // Known test case: fragment 2 of 5
      const hash = testHash(0xef);
      const data = new Uint8Array([10, 20, 30]);
      const res = createFragmentResponse(hash, data, 2, 5);
      const encoded = encodeResponse(res);

      const parsed = parseMessage(encoded);
      const body = parsed!.body as {
        h: Uint8Array;
        d: Uint8Array;
        i: number;
        n: number;
      };
      expect(body.i).toBe(2);
      expect(body.n).toBe(5);
    });
  });
});
