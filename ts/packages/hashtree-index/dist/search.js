import { BTree } from './btree.js';
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
    btree;
    stopWords;
    minKeywordLength;
    constructor(store, options = {}) {
        this.btree = new BTree(store, { order: options.order ?? 64 });
        this.stopWords = options.stopWords ?? DEFAULT_STOP_WORDS;
        this.minKeywordLength = options.minKeywordLength ?? DEFAULT_MIN_KEYWORD_LENGTH;
    }
    /**
     * Parse text into searchable keywords.
     * Filters stop words, short words, and pure numbers (except 4-digit years).
     */
    parseKeywords(text) {
        if (!text)
            return [];
        const keywords = [];
        const seen = new Set();
        for (const rawWord of text.split(/[^\p{L}\p{N}]+/u)) {
            if (!rawWord)
                continue;
            for (const word of this.expandKeywordVariants(rawWord)) {
                if (word.length >= this.minKeywordLength &&
                    !this.stopWords.has(word) &&
                    !this.isPureNumber(word) &&
                    !seen.has(word)) {
                    seen.add(word);
                    keywords.push(word);
                }
            }
        }
        return keywords;
    }
    expandKeywordVariants(rawWord) {
        const variants = new Set();
        const normalized = rawWord.toLowerCase();
        if (normalized) {
            variants.add(normalized);
        }
        const splitWord = rawWord
            .replace(/([\p{Lu}]+)([\p{Lu}][\p{Ll}])/gu, '$1 $2')
            .replace(/([\p{Ll}\p{N}])([\p{Lu}])/gu, '$1 $2')
            .replace(/([\p{L}])(\p{N})/gu, '$1 $2')
            .replace(/(\p{N})([\p{L}])/gu, '$1 $2');
        for (const part of splitWord.split(/\s+/)) {
            const normalizedPart = part.toLowerCase();
            if (normalizedPart) {
                variants.add(normalizedPart);
            }
        }
        return [...variants];
    }
    /**
     * Check if word is a pure number (excluding 4-digit years 1900-2099)
     */
    isPureNumber(word) {
        if (!/^\d+$/.test(word))
            return false;
        // Allow 4-digit years
        if (/^(19|20)\d{2}$/.test(word))
            return false;
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
    async index(root, prefix, terms, id, value) {
        let newRoot = root;
        for (const term of terms) {
            const key = `${prefix}${term}:${id}`;
            try {
                newRoot = await this.btree.insert(newRoot, key, value);
            }
            catch (e) {
                console.error('Failed to index term:', term, e);
            }
        }
        return newRoot;
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
    async search(root, prefix, query, options = {}) {
        return await this.searchTerms(root, prefix, this.parseKeywords(query), options);
    }
    /**
     * Search for items using caller-supplied normalized terms.
     * This is useful when the app wants custom term expansion without reimplementing ranking.
     */
    async searchTerms(root, prefix, terms, options = {}) {
        if (!root)
            return [];
        const limit = normalizeResultLimit(options.limit);
        if (limit === 0) {
            return [];
        }
        const keywords = normalizeSearchTerms(terms);
        if (keywords.length === 0) {
            return [];
        }
        const fullMatch = options.fullMatch ?? false;
        const scanLimit = normalizeScanLimit(options.scanLimit, limit);
        const results = await collectRankedMatches({
            prefix,
            keywords,
            fullMatch,
            scanLimit,
            iterate: (searchPrefix) => this.btree.prefix(root, searchPrefix),
            onError: (keyword, error) => {
                console.error('Search error for keyword:', keyword, error);
            },
        });
        return sortRankedMatches(results, limit)
            .map(([id, { value, score, exactMatches, prefixDistance }]) => ({
            id,
            value,
            score,
            exactMatches,
            prefixDistance,
        }));
    }
    /**
     * Remove an item from the index.
     * Must provide the same terms it was indexed under.
     */
    async remove(root, prefix, terms, id) {
        let newRoot = root;
        for (const term of terms) {
            const key = `${prefix}${term}:${id}`;
            try {
                newRoot = await this.btree.delete(newRoot, key);
                if (!newRoot)
                    break;
            }
            catch (e) {
                console.error('Failed to remove term:', term, e);
            }
        }
        return newRoot;
    }
    /**
     * Merge two search index roots.
     * @param preferOther - If true, prefer other's values on conflict (e.g., other is from newer event)
     */
    async merge(base, other, preferOther = false) {
        return this.btree.merge(base, other, preferOther);
    }
    async build(items) {
        return this.btree.build(items);
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
    async indexLink(root, prefix, terms, id, targetCid) {
        let newRoot = root;
        for (const term of terms) {
            const key = `${prefix}${term}:${id}`;
            try {
                newRoot = await this.btree.insertLink(newRoot, key, targetCid);
            }
            catch (e) {
                console.error('Failed to index link for term:', term, e);
            }
        }
        return newRoot;
    }
    async buildLinks(items) {
        return this.btree.buildLinks(items);
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
    async searchLinks(root, prefix, query, options = {}) {
        return await this.searchLinkTerms(root, prefix, this.parseKeywords(query), options);
    }
    /**
     * Search for CID links using caller-supplied normalized terms.
     */
    async searchLinkTerms(root, prefix, terms, options = {}) {
        if (!root)
            return [];
        const limit = normalizeResultLimit(options.limit);
        if (limit === 0) {
            return [];
        }
        const keywords = normalizeSearchTerms(terms);
        if (keywords.length === 0) {
            return [];
        }
        const fullMatch = options.fullMatch ?? false;
        const scanLimit = normalizeScanLimit(options.scanLimit, limit);
        const results = await collectRankedMatches({
            prefix,
            keywords,
            fullMatch,
            scanLimit,
            iterate: (searchPrefix) => this.btree.prefixLinks(root, searchPrefix),
            onError: (keyword, error) => {
                console.error('Search error for keyword:', keyword, error);
            },
        });
        return sortRankedMatches(results, limit)
            .map(([id, { value: cid, score, exactMatches, prefixDistance }]) => ({
            id,
            cid,
            score,
            exactMatches,
            prefixDistance,
        }));
    }
    /**
     * Merge two search index roots with CID links.
     * @param preferOther - If true, prefer other's CIDs on conflict
     */
    async mergeLinks(base, other, preferOther = false) {
        return this.btree.mergeLinks(base, other, preferOther);
    }
}
function normalizeResultLimit(limit) {
    if (limit === undefined) {
        return 20;
    }
    if (!Number.isFinite(limit)) {
        return Number.POSITIVE_INFINITY;
    }
    return Math.max(0, Math.floor(limit));
}
function normalizeScanLimit(scanLimit, limit) {
    const fallback = Number.isFinite(limit) ? limit * 2 : Number.POSITIVE_INFINITY;
    if (scanLimit === undefined) {
        return fallback;
    }
    if (!Number.isFinite(scanLimit)) {
        return Number.POSITIVE_INFINITY;
    }
    return Math.max(limit, Math.floor(scanLimit ?? fallback));
}
function normalizeSearchTerms(terms) {
    const seen = new Set();
    const normalized = [];
    for (const term of terms) {
        const nextTerm = `${term ?? ''}`.trim().toLowerCase();
        if (!nextTerm || seen.has(nextTerm)) {
            continue;
        }
        seen.add(nextTerm);
        normalized.push(nextTerm);
    }
    return normalized;
}
async function collectRankedMatches(options) {
    const results = new Map();
    for (const keyword of options.keywords) {
        try {
            const searchPrefix = `${options.prefix}${keyword}${options.fullMatch ? ':' : ''}`;
            let count = 0;
            for await (const [key, value] of options.iterate(searchPrefix)) {
                if (count++ >= options.scanLimit) {
                    break;
                }
                const afterPrefix = key.slice(options.prefix.length);
                const colonIndex = afterPrefix.indexOf(':');
                if (colonIndex === -1) {
                    continue;
                }
                const term = afterPrefix.slice(0, colonIndex);
                const id = afterPrefix.slice(colonIndex + 1);
                const exactMatch = term === keyword ? 1 : 0;
                const prefixDistance = Math.max(0, term.length - keyword.length);
                const existing = results.get(id);
                if (existing) {
                    existing.score += 1;
                    existing.exactMatches += exactMatch;
                    existing.prefixDistance += prefixDistance;
                }
                else {
                    results.set(id, {
                        value,
                        score: 1,
                        exactMatches: exactMatch,
                        prefixDistance,
                    });
                }
            }
        }
        catch (error) {
            options.onError(keyword, error);
        }
    }
    return results;
}
function sortRankedMatches(results, limit) {
    return [...results.entries()]
        .sort((left, right) => {
        if (right[1].score !== left[1].score) {
            return right[1].score - left[1].score;
        }
        if (right[1].exactMatches !== left[1].exactMatches) {
            return right[1].exactMatches - left[1].exactMatches;
        }
        if (left[1].prefixDistance !== right[1].prefixDistance) {
            return left[1].prefixDistance - right[1].prefixDistance;
        }
        return left[0].localeCompare(right[0]);
    })
        .slice(0, limit);
}
//# sourceMappingURL=search.js.map