import { describe, it, expect } from 'vitest';
import {
  nhashEncode,
  nhashDecode,
  npathEncode,
  npathDecode,
  decode,
  isNHash,
  isNPath,
  NHashTypeGuard,
  BECH32_REGEX,
  toHex,
  type NHashData,
  type NPathData,
} from '../src/index.js';

// Test vectors
const TEST_HASH = 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855'; // SHA256('')
const TEST_HASH_2 = 'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad'; // SHA256('abc')
const TEST_PUBKEY = '82341f882b6eabcd2ba7f1ef90aad961cf074af15b9ef44a09f9d2a8fbfbe6a2';
const TEST_DECRYPT_KEY = '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef';

describe('nhash - permalink', () => {
  describe('nhashEncode / nhashDecode', () => {
    it('should encode and decode simple hash string', () => {
      const encoded = nhashEncode(TEST_HASH);

      expect(encoded).toMatch(/^nhash1[a-z0-9]+$/);
      expect(isNHash(encoded)).toBe(true);

      const decoded = nhashDecode(encoded);
      expect(toHex(decoded.hash)).toBe(TEST_HASH);
      expect(decoded.key).toBeUndefined();
    });

    it('should accept Uint8Array hash', () => {
      const hashBytes = new Uint8Array(32).fill(0xab);
      const encoded = nhashEncode(hashBytes);

      expect(encoded).toMatch(/^nhash1[a-z0-9]+$/);

      const decoded = nhashDecode(encoded);
      expect(toHex(decoded.hash)).toBe('ab'.repeat(32));
    });

    it('should encode NHashData with only hash', () => {
      const encoded = nhashEncode({ hash: TEST_HASH });
      const decoded = nhashDecode(encoded);

      expect(toHex(decoded.hash)).toBe(TEST_HASH);
      expect(decoded.key).toBeUndefined();
    });

    it('should encode and decode hash with decrypt key', () => {
      const data: NHashData = {
        hash: TEST_HASH,
        decryptKey: TEST_DECRYPT_KEY,
      };

      const encoded = nhashEncode(data);
      expect(encoded).toMatch(/^nhash1[a-z0-9]+$/);

      const decoded = nhashDecode(encoded);
      expect(toHex(decoded.hash)).toBe(TEST_HASH);
      expect(decoded.key ? toHex(decoded.key) : undefined).toBe(TEST_DECRYPT_KEY);
    });

    it('should throw on invalid hash length', () => {
      expect(() => nhashEncode('abcd')).toThrow();
      expect(() => nhashEncode(new Uint8Array(16))).toThrow();
    });

    it('should produce consistent encoding', () => {
      const encoded1 = nhashEncode(TEST_HASH);
      const encoded2 = nhashEncode(TEST_HASH);
      expect(encoded1).toBe(encoded2);
    });

    it('should produce different encodings for different hashes', () => {
      const encoded1 = nhashEncode(TEST_HASH);
      const encoded2 = nhashEncode(TEST_HASH_2);
      expect(encoded1).not.toBe(encoded2);
    });

    it('should strip hashtree: URI prefix', () => {
      const encoded = nhashEncode(TEST_HASH);
      const decoded = nhashDecode(`hashtree:${encoded}`);
      expect(toHex(decoded.hash)).toBe(TEST_HASH);
    });

    it('should throw on wrong prefix', () => {
      // Create a valid npath and try to decode as nhash
      const validNpath = npathEncode({ pubkey: TEST_PUBKEY, treeName: 'test', path: [] });
      expect(() => nhashDecode(validNpath)).toThrow(/Expected nhash prefix/);
    });

    it('should roundtrip consistently', () => {
      const data: NHashData = {
        hash: TEST_HASH,
        decryptKey: TEST_DECRYPT_KEY,
      };

      const encoded = nhashEncode(data);
      const decoded = nhashDecode(encoded);
      const reencoded = nhashEncode(decoded);

      expect(reencoded).toBe(encoded);
    });

    // Note: Path is now kept in URL segments, not encoded in nhash
    // Example: /nhash1.../path/to/file.jpg
  });
});

describe('npath - live reference', () => {
  describe('npathEncode / npathDecode', () => {
    it('should encode and decode basic npath', () => {
      const data: NPathData = {
        pubkey: TEST_PUBKEY,
        treeName: 'home',
        path: ['photos', 'vacation', 'beach.jpg'],
      };

      const encoded = npathEncode(data);

      expect(encoded).toMatch(/^npath1[a-z0-9]+$/);
      expect(isNPath(encoded)).toBe(true);

      const decoded = npathDecode(encoded);
      expect(decoded.pubkey).toBe(TEST_PUBKEY);
      expect(decoded.treeName).toBe('home');
      expect(decoded.path).toEqual(['photos', 'vacation', 'beach.jpg']);
      expect(decoded.decryptKey).toBeUndefined();
    });

    it('should encode and decode with empty path', () => {
      const data: NPathData = {
        pubkey: TEST_PUBKEY,
        treeName: 'documents',
        path: [],
      };

      const encoded = npathEncode(data);
      const decoded = npathDecode(encoded);

      expect(decoded.pubkey).toBe(TEST_PUBKEY);
      expect(decoded.treeName).toBe('documents');
      expect(decoded.path).toEqual([]);
    });

    it('should encode and decode with decrypt key', () => {
      const data: NPathData = {
        pubkey: TEST_PUBKEY,
        treeName: 'encrypted',
        path: ['secret.txt'],
        decryptKey: TEST_DECRYPT_KEY,
      };

      const encoded = npathEncode(data);
      const decoded = npathDecode(encoded);

      expect(decoded.pubkey).toBe(TEST_PUBKEY);
      expect(decoded.treeName).toBe('encrypted');
      expect(decoded.path).toEqual(['secret.txt']);
      expect(decoded.decryptKey).toBe(TEST_DECRYPT_KEY);
    });

    it('should handle unicode in path and tree name', () => {
      const data: NPathData = {
        pubkey: TEST_PUBKEY,
        treeName: '我的文件',
        path: ['フォルダ', '文件.txt'],
      };

      const encoded = npathEncode(data);
      const decoded = npathDecode(encoded);

      expect(decoded.treeName).toBe('我的文件');
      expect(decoded.path).toEqual(['フォルダ', '文件.txt']);
    });

    it('should handle special characters in path', () => {
      const data: NPathData = {
        pubkey: TEST_PUBKEY,
        treeName: 'test-tree_123',
        path: ['folder with spaces', 'file-name_v2.0.txt'],
      };

      const encoded = npathEncode(data);
      const decoded = npathDecode(encoded);

      expect(decoded.treeName).toBe('test-tree_123');
      expect(decoded.path).toEqual(['folder with spaces', 'file-name_v2.0.txt']);
    });

    it('should throw on invalid pubkey', () => {
      expect(() => npathEncode({
        pubkey: 'short',
        treeName: 'test',
        path: [],
      })).toThrow();
    });

    it('should strip hashtree: URI prefix', () => {
      const data: NPathData = {
        pubkey: TEST_PUBKEY,
        treeName: 'test',
        path: [],
      };
      const encoded = npathEncode(data);
      const decoded = npathDecode(`hashtree:${encoded}`);
      expect(decoded.pubkey).toBe(TEST_PUBKEY);
    });

    it('should throw on wrong prefix', () => {
      // Create a valid nhash and try to decode as npath
      const validNhash = nhashEncode(TEST_HASH);
      expect(() => npathDecode(validNhash)).toThrow(/Expected npath prefix/);
    });

    it('should roundtrip consistently', () => {
      const data: NPathData = {
        pubkey: TEST_PUBKEY,
        treeName: 'my-tree',
        path: ['folder1', 'folder2', 'file.txt'],
        decryptKey: TEST_DECRYPT_KEY,
      };

      const encoded = npathEncode(data);
      const decoded = npathDecode(encoded);
      const reencoded = npathEncode(decoded);

      expect(reencoded).toBe(encoded);
    });
  });
});

describe('generic decode', () => {
  it('should decode nhash', () => {
    const encoded = nhashEncode(TEST_HASH);
    const result = decode(encoded);

    expect(result.type).toBe('nhash');
    if (result.type === 'nhash') {
      expect(toHex(result.data.hash)).toBe(TEST_HASH);
    }
  });

  it('should decode npath', () => {
    const encoded = npathEncode({
      pubkey: TEST_PUBKEY,
      treeName: 'home',
      path: ['test.txt'],
    });
    const result = decode(encoded);

    expect(result.type).toBe('npath');
    if (result.type === 'npath') {
      expect(result.data.pubkey).toBe(TEST_PUBKEY);
      expect(result.data.treeName).toBe('home');
    }
  });

  it('should strip hashtree: prefix', () => {
    const encoded = nhashEncode(TEST_HASH);
    const result = decode(`hashtree:${encoded}`);
    expect(result.type).toBe('nhash');
  });

  it('should throw on unknown prefix', () => {
    expect(() => decode('unknown1qqqqqq')).toThrow(/Unknown prefix/);
  });
});

describe('type guards', () => {
  it('should identify nhash strings', () => {
    const encoded = nhashEncode(TEST_HASH);
    expect(isNHash(encoded)).toBe(true);
    expect(isNPath(encoded)).toBe(false);
    expect(NHashTypeGuard.isNHash(encoded)).toBe(true);
  });

  it('should identify npath strings', () => {
    const encoded = npathEncode({
      pubkey: TEST_PUBKEY,
      treeName: 'test',
      path: [],
    });
    expect(isNHash(encoded)).toBe(false);
    expect(isNPath(encoded)).toBe(true);
    expect(NHashTypeGuard.isNPath(encoded)).toBe(true);
  });

  it('should return false for invalid strings', () => {
    expect(isNHash('')).toBe(false);
    expect(isNHash(null)).toBe(false);
    expect(isNHash(undefined)).toBe(false);
    expect(isNHash('npub1...')).toBe(false);
    expect(isNPath('')).toBe(false);
    expect(isNPath(null)).toBe(false);
  });
});

describe('BECH32_REGEX', () => {
  it('should match nhash strings', () => {
    const encoded = nhashEncode(TEST_HASH);
    expect(BECH32_REGEX.test(encoded)).toBe(true);
  });

  it('should match npath strings', () => {
    const encoded = npathEncode({
      pubkey: TEST_PUBKEY,
      treeName: 'test',
      path: ['file.txt'],
    });
    expect(BECH32_REGEX.test(encoded)).toBe(true);
  });
});
