import { type CID } from '@hashtree/core';
import { BTree } from './btree.js';
import { type RankedCandidate } from './ranked-candidates.js';
import { decodeDocumentStats } from './ranked-schema.js';
import { countMatchedPhrases, scoreBm25fTerm } from './ranked-score.js';
import type {
  ParsedRankedQuery,
  RankedDocumentStats,
  RankedSearchResult,
  RankedSearchSegmentManifest,
} from './ranked-types.js';

const READ_BATCH_SIZE = 64;

type ScoredCandidate = Omit<RankedSearchResult, 'value'>;
type PreparedCandidate = {
  id: string;
  candidate: RankedCandidate;
  matchedTerms: string[];
  matchedPhrases: number;
};

export async function scoreTopCandidates(options: {
  btree: BTree;
  documentsRoot: CID;
  candidates: ReadonlyMap<string, RankedCandidate>;
  parsed: ParsedRankedQuery;
  frequencies: ReadonlyMap<string, number>;
  fields: ReadonlyMap<string, RankedSearchSegmentManifest['fields'][number]>;
  selectedFields: ReadonlySet<string>;
  manifest: RankedSearchSegmentManifest;
  corpusDocuments: number;
  k1: number;
  limit: number;
}): Promise<ScoredCandidate[]> {
  const top: ScoredCandidate[] = [];
  let batch: PreparedCandidate[] = [];
  for (const [id, candidate] of options.candidates) {
    const prepared = prepareCandidate(id, candidate, options.parsed, options.selectedFields);
    if (!prepared) continue;
    batch.push(prepared);
    if (batch.length === READ_BATCH_SIZE) {
      await scoreBatch(batch, top, options);
      batch = [];
    }
  }
  if (batch.length > 0) await scoreBatch(batch, top, options);
  top.sort(compareScores);
  if (Number.isFinite(options.limit)) top.length = Math.min(top.length, options.limit);
  return top;
}

async function scoreBatch(
  batch: readonly PreparedCandidate[],
  top: ScoredCandidate[],
  options: Parameters<typeof scoreTopCandidates>[0],
): Promise<void> {
  const rawDocuments = await Promise.all(batch.map(({ id }) =>
    options.btree.get(options.documentsRoot, id)));
  for (let index = 0; index < batch.length; index += 1) {
    const item = batch[index];
    const rawDocument = rawDocuments[index];
    if (!rawDocument) throw new Error(`Missing ranked search document stats: ${item.id}`);
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

function prepareCandidate(
  id: string,
  candidate: RankedCandidate,
  parsed: ParsedRankedQuery,
  fields: ReadonlySet<string>,
): PreparedCandidate | null {
  const matchedTerms = parsed.terms.filter((term) => candidate.postings.has(term));
  const matchedPhrases = countMatchedPhrases(parsed.phrases, candidate.postings, fields);
  if (matchedPhrases !== parsed.phrases.length) return null;
  return { id, candidate, matchedTerms, matchedPhrases };
}

function scoreCandidate(options: {
  candidate: RankedCandidate;
  document: RankedDocumentStats;
  matchedTerms: readonly string[];
  frequencies: ReadonlyMap<string, number>;
  fields: ReadonlyMap<string, RankedSearchSegmentManifest['fields'][number]>;
  selectedFields: ReadonlySet<string>;
  corpusDocuments: number;
  k1: number;
}): number {
  let score = 0;
  for (const term of options.matchedTerms) {
    const posting = options.candidate.postings.get(term);
    const documentFrequency = options.frequencies.get(term);
    if (!posting || documentFrequency === undefined) continue;
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

function validateDocumentFields(
  document: RankedDocumentStats,
  manifest: RankedSearchSegmentManifest,
  id: string,
): void {
  for (const field of manifest.fields) {
    if (document.lengths[field.name] === undefined) {
      throw new Error(`Missing ranked search field length for ${id}: ${field.name}`);
    }
  }
}

function retainTop(top: ScoredCandidate[], candidate: ScoredCandidate, limit: number): void {
  top.push(candidate);
  if (Number.isFinite(limit) && top.length > limit * 2) {
    top.sort(compareScores);
    top.length = limit;
  }
}

function compareScores(left: ScoredCandidate, right: ScoredCandidate): number {
  return right.score - left.score || compareStrings(left.id, right.id);
}

function compareStrings(left: string, right: string): number {
  return left < right ? -1 : left > right ? 1 : 0;
}
