import { collectRankedCandidates, hasMissingRequiredTerm, loadTermFrequencies, } from './ranked-candidates.js';
import { scoreTopCandidates } from './ranked-ranking.js';
import { prepareRankedScoringContext } from './ranked-scoring-context.js';
import { readRankedSegment } from './ranked-segment.js';
import { parseRankedQuery } from './ranked-tokenize.js';
import { queryRankedTopK } from './ranked-top-k-query.js';
export async function queryRankedSegment(btree, tree, root, query, options) {
    const limit = normalizeLimit(options.limit);
    if (limit === 0)
        return [];
    const parsed = parseRankedQuery(query);
    if (parsed.terms.length === 0)
        return [];
    const { manifest, roots, topKManifest } = await readRankedSegment(tree, root);
    if (manifest.documentCount === 0 || manifest.postingCount === 0)
        return [];
    if (!roots.documents)
        throw new Error('Missing ranked search document index');
    if (!roots.postings || !roots.terms)
        throw new Error('Missing ranked search postings index');
    if (manifest.storedValueCount > 0 && !roots.values) {
        throw new Error('Missing ranked search values index');
    }
    const fields = new Map(manifest.fields.map((field) => [field.name, field]));
    const selectedFields = selectFields(fields, options.fields);
    if (selectedFields.size === 0)
        return [];
    const operator = normalizeOperator(options.operator);
    const localTermFrequencies = await loadTermFrequencies(btree, roots.terms, parsed.terms, selectedFields, fields, manifest.documentCount);
    const localFrequencies = localTermFrequencies.selected;
    const scoring = options.scoringContext
        ? prepareRankedScoringContext(options.scoringContext, manifest, parsed.terms, selectedFields, localFrequencies)
        : {
            corpusDocuments: manifest.documentCount,
            k1: manifest.k1,
            fields,
            frequencies: localFrequencies,
        };
    if (hasMissingRequiredTerm(parsed, localFrequencies, operator))
        return [];
    const top = roots.topK && topKManifest && Number.isFinite(limit) && operator === 'or'
        ? await queryRankedTopK({
            btree,
            tree,
            topKRoots: roots.topK,
            topKManifest,
            postingsRoot: roots.postings,
            documentsRoot: roots.documents,
            parsed,
            localFrequencies,
            localDocumentFrequencies: localTermFrequencies.all,
            frequencies: scoring.frequencies,
            fields: scoring.fields,
            selectedFields,
            manifest,
            corpusDocuments: scoring.corpusDocuments,
            k1: scoring.k1,
            operator,
            limit,
        })
        : await queryUnbounded(btree, roots.postings, roots.documents, parsed, localFrequencies, scoring.frequencies, scoring.fields, selectedFields, manifest, scoring.corpusDocuments, scoring.k1, operator, limit);
    if (top.length === 0)
        return [];
    return await Promise.all(top.map(async (result) => {
        const value = roots.values ? await btree.get(roots.values, result.id) : null;
        return { ...result, ...(value !== null ? { value } : {}) };
    }));
}
async function queryUnbounded(btree, postingsRoot, documentsRoot, parsed, localFrequencies, frequencies, fields, selectedFields, manifest, corpusDocuments, k1, operator, limit) {
    const candidates = await collectRankedCandidates(btree, postingsRoot, parsed.terms, localFrequencies, selectedFields, operator);
    if (candidates.size === 0)
        return [];
    return await scoreTopCandidates({
        btree,
        documentsRoot,
        candidates,
        parsed,
        frequencies,
        fields,
        selectedFields,
        manifest,
        corpusDocuments,
        k1,
        limit,
    });
}
function selectFields(available, requested) {
    if (!requested)
        return new Set(available.keys());
    const selected = new Set();
    for (const field of requested) {
        if (!available.has(field))
            throw new Error(`Unknown ranked search field: ${field}`);
        selected.add(field);
    }
    return selected;
}
function normalizeOperator(operator) {
    if (operator === undefined)
        return 'or';
    if (operator !== 'or' && operator !== 'and') {
        throw new Error(`Invalid ranked search operator: ${String(operator)}`);
    }
    return operator;
}
function normalizeLimit(limit) {
    if (limit === undefined)
        return 20;
    if (limit === Number.POSITIVE_INFINITY)
        return Number.POSITIVE_INFINITY;
    if (!Number.isFinite(limit))
        throw new Error('Invalid ranked search limit');
    return Math.max(0, Math.floor(limit));
}
//# sourceMappingURL=ranked-query.js.map