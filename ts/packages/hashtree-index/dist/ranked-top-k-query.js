import { postingMatchesFields } from './ranked-candidates.js';
import { scoreTopCandidates } from './ranked-ranking.js';
import { decodePosting } from './ranked-schema.js';
import { scoreBm25fTerm } from './ranked-score.js';
import { RANKED_TOP_K_MIN_DOCUMENT_FREQUENCY, readRankedTopKNode, } from './ranked-top-k.js';
export async function queryRankedTopK(options) {
    validateTopKManifest(options.topKManifest, options.manifest);
    const frontier = [];
    const unacceleratedTerms = [];
    for (const term of options.parsed.terms) {
        if (!options.localFrequencies.has(term))
            continue;
        const cid = await options.btree.getLink(options.topKRoots, term);
        const expectedCount = options.localDocumentFrequencies.get(term);
        if (expectedCount === undefined) {
            throw new Error(`Inconsistent ranked top-k term coverage: ${term}`);
        }
        if (!cid) {
            if (expectedCount >= options.topKManifest.minimumDocumentFrequency) {
                throw new Error(`Missing ranked top-k term root: ${term}`);
            }
            unacceleratedTerms.push(term);
            continue;
        }
        if (expectedCount < options.topKManifest.minimumDocumentFrequency) {
            throw new Error(`Unexpected ranked top-k term root: ${term}`);
        }
        const loaded = await readRankedTopKNode(options.tree, cid);
        if (loaded.summary.count !== expectedCount) {
            throw new Error(`Inconsistent ranked top-k term coverage: ${term}`);
        }
        validateSummaryFields(loaded.summary, options.manifest, term);
        frontier.push({
            term,
            cid,
            summary: loaded.summary,
            upperBound: scoreUpperBound(term, loaded.summary, options),
            loaded,
        });
    }
    const evaluated = new Set();
    const unacceleratedIds = await collectUnacceleratedIds(unacceleratedTerms, options);
    const initialCandidates = await loadCandidates(unacceleratedIds, evaluated, options);
    const top = await scoreTopCandidates({
        btree: options.btree,
        documentsRoot: options.documentsRoot,
        candidates: initialCandidates,
        parsed: options.parsed,
        frequencies: options.frequencies,
        fields: options.fields,
        selectedFields: options.selectedFields,
        manifest: options.manifest,
        corpusDocuments: options.corpusDocuments,
        k1: options.k1,
        limit: options.limit,
    });
    while (frontier.length > 0) {
        frontier.sort(compareFrontier);
        const current = frontier.shift();
        if (!current)
            break;
        const node = current.loaded ?? await readRankedTopKNode(options.tree, current.cid);
        if (!summariesEqual(current.summary, node.summary)) {
            throw new Error(`Inconsistent ranked top-k node summary: ${current.term}`);
        }
        if (node.kind === 'internal') {
            for (const child of node.children) {
                validateSummaryFields(child.summary, options.manifest, current.term);
                frontier.push({
                    term: current.term,
                    cid: child.cid,
                    summary: child.summary,
                    upperBound: scoreUpperBound(current.term, child.summary, options),
                });
            }
        }
        else {
            const candidates = await loadCandidates(node.ids, evaluated, options);
            const scored = await scoreTopCandidates({
                btree: options.btree,
                documentsRoot: options.documentsRoot,
                candidates,
                parsed: options.parsed,
                frequencies: options.frequencies,
                fields: options.fields,
                selectedFields: options.selectedFields,
                manifest: options.manifest,
                corpusDocuments: options.corpusDocuments,
                k1: options.k1,
                limit: options.limit,
            });
            top.push(...scored);
            top.sort(compareScores);
            top.length = Math.min(top.length, options.limit);
        }
        if (canStop(top, frontier, options.limit))
            break;
    }
    top.sort(compareScores);
    return top;
}
async function collectUnacceleratedIds(terms, options) {
    const ids = new Set();
    for (const term of terms) {
        const prefix = `${term}\0`;
        let total = 0;
        let selected = 0;
        for await (const [key, raw] of options.btree.prefix(options.postingsRoot, prefix)) {
            total += 1;
            const posting = decodePosting(raw);
            if (!postingMatchesFields(posting, options.selectedFields))
                continue;
            selected += 1;
            ids.add(key.slice(prefix.length));
        }
        if (total !== options.localDocumentFrequencies.get(term)
            || selected !== options.localFrequencies.get(term)) {
            throw new Error(`Inconsistent unaccelerated ranked term coverage: ${term}`);
        }
    }
    return [...ids];
}
async function loadCandidates(ids, evaluated, options) {
    const pending = ids.filter((id) => !evaluated.has(id));
    for (const id of pending)
        evaluated.add(id);
    const loaded = await Promise.all(pending.map(async (id) => {
        const rawPostings = await Promise.all(options.parsed.terms.map((term) => options.btree.get(options.postingsRoot, postingKey(term, id))));
        const postings = new Map();
        for (let index = 0; index < options.parsed.terms.length; index += 1) {
            const raw = rawPostings[index];
            if (raw === null)
                continue;
            const posting = decodePosting(raw);
            if (postingMatchesFields(posting, options.selectedFields)) {
                postings.set(options.parsed.terms[index], posting);
            }
        }
        return [id, { postings }];
    }));
    const candidates = new Map();
    for (const [id, candidate] of loaded) {
        if (candidate.postings.size === 0)
            continue;
        if (options.operator === 'and'
            && options.parsed.terms.some((term) => !candidate.postings.has(term))) {
            continue;
        }
        candidates.set(id, candidate);
    }
    return candidates;
}
function scoreUpperBound(term, summary, options) {
    const documentFrequency = options.frequencies.get(term);
    if (documentFrequency === undefined)
        return 0;
    const postingFields = Object.create(null);
    const lengths = Object.create(null);
    for (const [fieldName, bound] of Object.entries(summary.fields)) {
        if (!options.selectedFields.has(fieldName))
            continue;
        postingFields[fieldName] = { frequency: bound.maxFrequency, positions: [] };
        lengths[fieldName] = bound.minLength;
    }
    if (Object.keys(postingFields).length === 0)
        return 0;
    return conservativeUpperBound(scoreBm25fTerm({
        posting: { fields: postingFields },
        document: { lengths },
        fields: options.fields,
        selectedFields: options.selectedFields,
        corpusDocuments: options.corpusDocuments,
        documentFrequency,
        k1: options.k1,
    }));
}
function canStop(top, frontier, limit) {
    const maxima = new Map();
    for (const node of frontier) {
        maxima.set(node.term, Math.max(maxima.get(node.term) ?? 0, node.upperBound));
    }
    let unseenUpperBound = 0;
    for (const maximum of maxima.values()) {
        unseenUpperBound = conservativeUpperBound(unseenUpperBound + maximum);
    }
    if (top.length < limit)
        return unseenUpperBound === 0;
    return top[limit - 1].score > unseenUpperBound;
}
function validateTopKManifest(topK, segment) {
    if (topK.minimumDocumentFrequency !== RANKED_TOP_K_MIN_DOCUMENT_FREQUENCY
        || topK.termCount > segment.termCount
        || topK.postingCount > segment.postingCount
        || topK.termCount === 0
        || topK.postingCount === 0
        || topK.blockCount === 0) {
        throw new Error('Inconsistent ranked top-k manifest coverage');
    }
}
function conservativeUpperBound(value) {
    if (value === 0 || !Number.isFinite(value))
        return value;
    let upper = value;
    for (let count = 0; count < 16; count += 1)
        upper = nextUp(upper);
    return upper;
}
function nextUp(value) {
    const view = new DataView(new ArrayBuffer(8));
    view.setFloat64(0, value, false);
    view.setBigUint64(0, view.getBigUint64(0, false) + 1n, false);
    return view.getFloat64(0, false);
}
function validateSummaryFields(summary, segment, term) {
    const fields = new Set(segment.fields.map((field) => field.name));
    for (const [fieldName, bound] of Object.entries(summary.fields)) {
        if (!fields.has(fieldName)
            || bound.minLength > segment.maxTokensPerField
            || bound.maxFrequency > segment.maxTokensPerField) {
            throw new Error(`Invalid ranked top-k field for ${term}: ${fieldName}`);
        }
    }
}
function compareFrontier(left, right) {
    return right.upperBound - left.upperBound
        || compareStrings(left.term, right.term)
        || compareStrings(left.summary.minId, right.summary.minId);
}
function compareScores(left, right) {
    return right.score - left.score || compareStrings(left.id, right.id);
}
function summariesEqual(left, right) {
    return JSON.stringify(left) === JSON.stringify(right);
}
function postingKey(term, id) {
    return `${term}\0${id}`;
}
function compareStrings(left, right) {
    return left < right ? -1 : left > right ? 1 : 0;
}
//# sourceMappingURL=ranked-top-k-query.js.map