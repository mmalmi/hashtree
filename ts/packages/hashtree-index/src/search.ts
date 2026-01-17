import type { CID, Store } from 'hashtree';
import { BTree } from './btree.js';

export interface SearchIndexOptions {
  /** Max entries per B-Tree node. Default: 64 */
  order?: number;
  /** Words to exclude from indexing. Default: common English stop words */
  stopWords?: Set<string>;
  /** Minimum keyword length. Default: 2 */
  minKeywordLength?: number;
}

export interface SearchResult {
  id: string;
  value: string;
  score: number;
}

export interface SearchLinkResult {
  id: string;
  cid: CID;
  score: number;
}

export interface SearchOptions {
  /** Max results to return. Default: 20 */
  limit?: number;
  /** Require full keyword match vs prefix match. Default: false (prefix) */
  fullMatch?: boolean;
}

// Default English stop words
const DEFAULT_STOP_WORDS = new Set([
  'a', 'an', 'the', 'and', 'or', 'but', 'in', 'on', 'at', 'to', 'for',
  'of', 'with', 'by', 'from', 'is', 'it', 'as', 'be', 'was', 'are',
  'this', 'that', 'these', 'those', 'i', 'you', 'he', 'she', 'we', 'they',
  'my', 'your', 'his', 'her', 'its', 'our', 'their', 'what', 'which',
  'who', 'whom', 'how', 'when', 'where', 'why', 'will', 'would', 'could',
  'should', 'can', 'may', 'might', 'must', 'have', 'has', 'had', 'do',
  'does', 'did', 'been', 'being', 'get', 'got', 'just', 'now', 'then',
  'so', 'if', 'not', 'no', 'yes', 'all', 'any', 'some', 'more', 'most',
  'other', 'into', 'over', 'after', 'before', 'about', 'up', 'down',
  'out', 'off', 'through', 'during', 'under', 'again', 'further', 'once',
]);

const DEFAULT_MIN_KEYWORD_LENGTH = 2;

export class SearchIndex {
  private btree: BTree;
  private stopWords: Set<string>;
  private minKeywordLength: number;

  constructor(store: Store, options: SearchIndexOptions = {}) {
    this.btree = new BTree(store, { order: options.order ?? 64 });
    this.stopWords = options.stopWords ?? DEFAULT_STOP_WORDS;
    this.minKeywordLength = options.minKeywordLength ?? DEFAULT_MIN_KEYWORD_LENGTH;
  }

  /**
   * Parse text into searchable keywords.
   * Filters stop words, short words, and pure numbers (except 4-digit years).
   */
  parseKeywords(text: string): string[] {
    if (!text) return [];

    const words = text
      .toLowerCase()
      .split(/[^a-z0-9\u00C0-\u024F\u1E00-\u1EFF]+/i)
      .filter(word =>
        word.length >= this.minKeywordLength &&
        !this.stopWords.has(word) &&
        !this.isPureNumber(word)
      );

    return [...new Set(words)];
  }

  /**
   * Check if word is a pure number (excluding 4-digit years 1900-2099)
   */
  private isPureNumber(word: string): boolean {
    if (!/^\d+$/.test(word)) return false;
    // Allow 4-digit years
    if (/^(19|20)\d{2}$/.test(word)) return false;
    return true;
  }

  /**
   * Index an item under multiple terms.
   *
   * @param root Current index root (null for new index)
   * @param prefix Namespace prefix (e.g., "v:" for videos, "u:" for users)
   * @param terms Search terms to index under
   * @param id Unique identifier for deduplication
   * @param value JSON-serialized value to store
   * @returns New index root CID
   */
  async index(
    root: CID | null,
    prefix: string,
    terms: string[],
    id: string,
    value: string
  ): Promise<CID> {
    let newRoot = root;

    for (const term of terms) {
      const key = `${prefix}${term}:${id}`;
      try {
        newRoot = await this.btree.insert(newRoot, key, value);
      } catch (e) {
        console.error('Failed to index term:', term, e);
      }
    }

    return newRoot!;
  }

  /**
   * Search for items matching query terms.
   * Returns results sorted by score (number of matching terms) then by id.
   *
   * @param root Index root CID
   * @param prefix Namespace prefix to search within
   * @param query Search query text
   * @param options Search options (limit, fullMatch)
   */
  async search(
    root: CID | null,
    prefix: string,
    query: string,
    options: SearchOptions = {}
  ): Promise<SearchResult[]> {
    if (!root) return [];

    const { limit = 20, fullMatch = false } = options;
    const keywords = this.parseKeywords(query);

    // If parseKeywords filtered everything (stop words only), return empty
    // This ensures stop word queries don't match
    if (keywords.length === 0) {
      return [];
    }

    const results = new Map<string, { value: string; score: number }>();

    for (const keyword of keywords) {
      try {
        const searchPrefix = `${prefix}${keyword}${fullMatch ? ':' : ''}`;
        let count = 0;

        for await (const [key, value] of this.btree.prefix(root, searchPrefix)) {
          if (count++ >= limit * 2) break;

          // Extract id from key: prefix + term + ":" + id
          const afterPrefix = key.slice(prefix.length);
          const colonIndex = afterPrefix.indexOf(':');
          if (colonIndex === -1) continue;
          const id = afterPrefix.slice(colonIndex + 1);

          const existing = results.get(id);
          if (existing) {
            existing.score += 1;
          } else {
            results.set(id, { value, score: 1 });
          }
        }
      } catch (e) {
        console.error('Search error for keyword:', keyword, e);
      }
    }

    // Sort by score desc, then by id for stability
    const sorted = [...results.entries()]
      .sort((a, b) => {
        if (b[1].score !== a[1].score) return b[1].score - a[1].score;
        return a[0].localeCompare(b[0]);
      })
      .slice(0, limit);

    return sorted.map(([id, { value, score }]) => ({ id, value, score }));
  }

  /**
   * Remove an item from the index.
   * Must provide the same terms it was indexed under.
   */
  async remove(
    root: CID,
    prefix: string,
    terms: string[],
    id: string
  ): Promise<CID | null> {
    let newRoot: CID | null = root;

    for (const term of terms) {
      const key = `${prefix}${term}:${id}`;
      try {
        newRoot = await this.btree.delete(newRoot!, key);
        if (!newRoot) break;
      } catch (e) {
        console.error('Failed to remove term:', term, e);
      }
    }

    return newRoot;
  }

  /**
   * Merge two search index roots.
   * @param preferOther - If true, prefer other's values on conflict (e.g., other is from newer event)
   */
  async merge(base: CID | null, other: CID | null, preferOther = false): Promise<CID | null> {
    return this.btree.merge(base, other, preferOther);
  }

  // ============ CID Link Methods ============

  /**
   * Index an item with a CID link instead of string value.
   * Uses natural deduplication - same id will overwrite previous CID.
   *
   * @param root Current index root (null for new index)
   * @param prefix Namespace prefix (e.g., "v:" for videos)
   * @param terms Search terms to index under
   * @param id Unique identifier for deduplication (e.g., "pubkey:treeName")
   * @param targetCid CID to link to (e.g., video directory CID)
   * @returns New index root CID
   */
  async indexLink(
    root: CID | null,
    prefix: string,
    terms: string[],
    id: string,
    targetCid: CID
  ): Promise<CID> {
    let newRoot = root;

    for (const term of terms) {
      const key = `${prefix}${term}:${id}`;
      try {
        newRoot = await this.btree.insertLink(newRoot, key, targetCid);
      } catch (e) {
        console.error('Failed to index link for term:', term, e);
      }
    }

    return newRoot!;
  }

  /**
   * Search for CID links matching query terms.
   * Returns results sorted by score (number of matching terms).
   *
   * @param root Index root CID
   * @param prefix Namespace prefix to search within
   * @param query Search query text
   * @param options Search options (limit, fullMatch)
   */
  async searchLinks(
    root: CID | null,
    prefix: string,
    query: string,
    options: SearchOptions = {}
  ): Promise<SearchLinkResult[]> {
    if (!root) return [];

    const { limit = 20, fullMatch = false } = options;
    const keywords = this.parseKeywords(query);
    if (keywords.length === 0) return [];

    const results = new Map<string, { cid: CID; score: number }>();

    for (const keyword of keywords) {
      try {
        const searchPrefix = `${prefix}${keyword}${fullMatch ? ':' : ''}`;
        let count = 0;

        for await (const [key, cid] of this.btree.prefixLinks(root, searchPrefix)) {
          if (count++ >= limit * 2) break;

          // Extract id from key: prefix + term + ":" + id
          const afterPrefix = key.slice(prefix.length);
          const colonIndex = afterPrefix.indexOf(':');
          if (colonIndex === -1) continue;
          const id = afterPrefix.slice(colonIndex + 1);

          const existing = results.get(id);
          if (existing) {
            existing.score += 1;
          } else {
            results.set(id, { cid, score: 1 });
          }
        }
      } catch (e) {
        console.error('Search error for keyword:', keyword, e);
      }
    }

    // Sort by score desc, then by id for stability
    const sorted = [...results.entries()]
      .sort((a, b) => {
        if (b[1].score !== a[1].score) return b[1].score - a[1].score;
        return a[0].localeCompare(b[0]);
      })
      .slice(0, limit);

    return sorted.map(([id, { cid, score }]) => ({ id, cid, score }));
  }

  /**
   * Merge two search index roots with CID links.
   * @param preferOther - If true, prefer other's CIDs on conflict
   */
  async mergeLinks(base: CID | null, other: CID | null, preferOther = false): Promise<CID | null> {
    return this.btree.mergeLinks(base, other, preferOther);
  }
}
