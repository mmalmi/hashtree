import { decodeDocumentStats } from './ranked-schema.js';
import { countMatchedPhrases, scoreBm25fTerm } from './ranked-score.js';
const READ_BATCH_SIZE = 64;
export async function scoreTopCandidates(options) {
    const top = [];
    let batch = [];
    for (const [id, candidate] of options.candidates) {
        const prepared = prepareCandidate(id, candidate, options.parsed, options.selectedFields);
        if (!prepared)
            continue;
        batch.push(prepared);
        if (batch.length === READ_BATCH_SIZE) {
            await scoreBatch(batch, top, options);
            batch = [];
        }
    }
    if (batch.length > 0)
        await scoreBatch(batch, top, options);
    top.sort(compareScores);
    if (Number.isFinite(options.limit))
        top.length = Math.min(top.length, options.limit);
    return top;
}
async function scoreBatch(batch, top, options) {
    const rawDocuments = await Promise.all(batch.map(({ id }) => options.btree.get(options.documentsRoot, id)));
    for (let index = 0; index < batch.length; index += 1) {
        const item = batch[index];
        const rawDocument = rawDocuments[index];
        if (!rawDocument)
            throw new Error(`Missing ranked search document stats: ${item.id}`);
        const document = decodeDocumentStats(rawDocument);
        validateDocumentFields(document, options.manifest, item.id);
        retainTop(top, {
            id: item.id,
            score: scoreCandidate({
                candidate: item.candidate,
                document,
                matchedTerms: item.matchedTerms,
                frequencies: options.frequencies,
                fields: options.fields,
                selectedFields: options.selectedFields,
                corpusDocuments: options.corpusDocuments,
                k1: options.k1,
            }),
            matchedTerms: item.matchedTerms,
            matchedPhrases: item.matchedPhrases,
        }, options.limit);
    }
}
function prepareCandidate(id, candidate, parsed, fields) {
    const matchedTerms = parsed.terms.filter((term) => candidate.postings.has(term));
    const matchedPhrases = countMatchedPhrases(parsed.phrases, candidate.postings, fields);
    if (matchedPhrases !== parsed.phrases.length)
        return null;
    return { id, candidate, matchedTerms, matchedPhrases };
}
function scoreCandidate(options) {
    let score = 0;
    for (const term of options.matchedTerms) {
        const posting = options.candidate.postings.get(term);
        const documentFrequency = options.frequencies.get(term);
        if (!posting || documentFrequency === undefined)
            continue;
        score += scoreBm25fTerm({
            posting,
            document: options.document,
            fields: options.fields,
            selectedFields: options.selectedFields,
            corpusDocuments: options.corpusDocuments,
            documentFrequency,
            k1: options.k1,
        });
    }
    return score;
}
function validateDocumentFields(document, manifest, id) {
    for (const field of manifest.fields) {
        if (document.lengths[field.name] === undefined) {
            throw new Error(`Missing ranked search field length for ${id}: ${field.name}`);
        }
    }
}
function retainTop(top, candidate, limit) {
    top.push(candidate);
    if (Number.isFinite(limit) && top.length > limit * 2) {
        top.sort(compareScores);
        top.length = limit;
    }
}
function compareScores(left, right) {
    return right.score - left.score || compareStrings(left.id, right.id);
}
function compareStrings(left, right) {
    return left < right ? -1 : left > right ? 1 : 0;
}
//# sourceMappingURL=ranked-ranking.js.map