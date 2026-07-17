import { decodePosting, decodeTermStats } from './ranked-schema.js';
const POSTING_SEPARATOR = '\0';
const READ_BATCH_SIZE = 64;
export async function loadTermFrequencies(btree, termsRoot, terms, selectedFields, configuredFields, documentCount) {
    const frequencies = new Map();
    const documentFrequencies = new Map();
    const rawStats = await Promise.all(terms.map((term) => btree.get(termsRoot, term)));
    for (let index = 0; index < terms.length; index += 1) {
        const raw = rawStats[index];
        if (!raw)
            continue;
        const term = terms[index];
        const stats = decodeTermStats(raw);
        validateTermFields(stats, configuredFields, term);
        if (stats.documentFrequency > documentCount) {
            throw new Error(`Invalid ranked search document frequency: ${term}`);
        }
        documentFrequencies.set(term, stats.documentFrequency);
        const frequency = stats.fieldSets
            .filter((fieldSet) => fieldSet.fields.some((field) => selectedFields.has(field)))
            .reduce((total, fieldSet) => total + fieldSet.documentFrequency, 0);
        if (frequency > documentCount) {
            throw new Error(`Invalid ranked search document frequency: ${term}`);
        }
        if (frequency > 0)
            frequencies.set(term, frequency);
    }
    return { selected: frequencies, all: documentFrequencies };
}
export function hasMissingRequiredTerm(parsed, frequencies, operator) {
    if (operator === 'and' && parsed.terms.some((term) => !frequencies.has(term)))
        return true;
    return parsed.phrases.some((phrase) => phrase.some((term) => !frequencies.has(term)));
}
export async function collectRankedCandidates(btree, postingsRoot, terms, frequencies, fields, operator) {
    return operator === 'and'
        ? await collectAndCandidates(btree, postingsRoot, terms, frequencies, fields)
        : await collectOrCandidates(btree, postingsRoot, terms, frequencies, fields);
}
async function collectOrCandidates(btree, postingsRoot, terms, frequencies, fields) {
    const candidates = new Map();
    for (const term of terms) {
        if (!frequencies.has(term))
            continue;
        const prefix = `${term}${POSTING_SEPARATOR}`;
        for await (const [key, rawPosting] of btree.prefix(postingsRoot, prefix)) {
            const posting = decodePosting(rawPosting);
            if (!postingMatchesFields(posting, fields))
                continue;
            const id = key.slice(prefix.length);
            const candidate = candidates.get(id) ?? { postings: new Map() };
            candidate.postings.set(term, posting);
            candidates.set(id, candidate);
        }
    }
    return candidates;
}
async function collectAndCandidates(btree, postingsRoot, terms, frequencies, fields) {
    const rankedTerms = [...terms].sort((left, right) => (frequencies.get(left) ?? 0) - (frequencies.get(right) ?? 0)
        || compareStrings(left, right));
    const candidates = await collectOrCandidates(btree, postingsRoot, rankedTerms.slice(0, 1), frequencies, fields);
    for (const term of rankedTerms.slice(1)) {
        const ids = [...candidates.keys()];
        for (let offset = 0; offset < ids.length; offset += READ_BATCH_SIZE) {
            const batch = ids.slice(offset, offset + READ_BATCH_SIZE);
            const postings = await Promise.all(batch.map((id) => btree.get(postingsRoot, postingKey(term, id))));
            for (let index = 0; index < batch.length; index += 1) {
                const id = batch[index];
                const rawPosting = postings[index];
                if (!rawPosting) {
                    candidates.delete(id);
                    continue;
                }
                const posting = decodePosting(rawPosting);
                if (!postingMatchesFields(posting, fields)) {
                    candidates.delete(id);
                    continue;
                }
                candidates.get(id)?.postings.set(term, posting);
            }
        }
        if (candidates.size === 0)
            break;
    }
    return candidates;
}
function validateTermFields(stats, fields, term) {
    for (const fieldSet of stats.fieldSets) {
        for (const field of fieldSet.fields) {
            if (!fields.has(field)) {
                throw new Error(`Unknown ranked search term field for ${term}: ${field}`);
            }
        }
    }
}
export function postingMatchesFields(posting, fields) {
    return Object.keys(posting.fields).some((field) => fields.has(field));
}
function postingKey(term, id) {
    return `${term}${POSTING_SEPARATOR}${id}`;
}
function compareStrings(left, right) {
    return left < right ? -1 : left > right ? 1 : 0;
}
//# sourceMappingURL=ranked-candidates.js.map