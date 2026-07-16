const TOKEN_PATTERN = /#([\p{L}\p{N}\p{M}_]+)|[\p{L}\p{N}][\p{L}\p{N}\p{M}]*(?:['’][\p{L}\p{N}\p{M}]+)*/gu;
const QUOTED_PHRASE_PATTERN = /"([^"]*)"/g;
export function tokenizeRankedField(value, maxTokens) {
    const values = rankedFieldValues(value);
    const occurrences = [];
    let length = 0;
    let position = 0;
    for (const text of values) {
        for (const token of lexicalTokens(text)) {
            if (length >= maxTokens) {
                return { length, occurrences };
            }
            occurrences.push({ term: token.term, position });
            if (token.hashtag) {
                occurrences.push({ term: `#${token.term}`, position });
            }
            length += 1;
            position += 1;
        }
        position += 1;
    }
    return { length, occurrences };
}
function rankedFieldValues(value) {
    if (value === undefined)
        return [];
    if (typeof value === 'string')
        return [value];
    if (Array.isArray(value) && value.every((item) => typeof item === 'string'))
        return value;
    throw new Error('Ranked search field values must be strings or string arrays');
}
export function parseRankedQuery(query) {
    const normalizedQuery = normalizeRankedText(query).replace(/[“”]/g, '"');
    const terms = [];
    const phrases = [];
    const seen = new Set();
    let cursor = 0;
    QUOTED_PHRASE_PATTERN.lastIndex = 0;
    for (const match of normalizedQuery.matchAll(QUOTED_PHRASE_PATTERN)) {
        const start = match.index ?? 0;
        appendQueryTerms(normalizedQuery.slice(cursor, start), terms, seen);
        const phrase = queryTerms(match[1] ?? '');
        if (phrase.length > 0) {
            phrases.push(phrase);
            appendUnique(phrase, terms, seen);
        }
        cursor = start + match[0].length;
    }
    appendQueryTerms(normalizedQuery.slice(cursor), terms, seen);
    return { terms, phrases };
}
export function normalizeRankedText(text) {
    return text.normalize('NFKC').toLowerCase().replace(/’/g, "'");
}
function appendQueryTerms(fragment, terms, seen) {
    appendUnique(queryTerms(fragment), terms, seen);
}
function appendUnique(source, target, seen) {
    for (const term of source) {
        if (!seen.has(term)) {
            seen.add(term);
            target.push(term);
        }
    }
}
function queryTerms(text) {
    return lexicalTokens(text).map((token) => token.hashtag ? `#${token.term}` : token.term);
}
function lexicalTokens(text) {
    const normalized = normalizeRankedText(text);
    const tokens = [];
    TOKEN_PATTERN.lastIndex = 0;
    for (const match of normalized.matchAll(TOKEN_PATTERN)) {
        const hashtag = match[1] !== undefined;
        const term = hashtag ? match[1] : match[0];
        if (term) {
            tokens.push({ term, hashtag });
        }
    }
    return tokens;
}
//# sourceMappingURL=ranked-tokenize.js.map