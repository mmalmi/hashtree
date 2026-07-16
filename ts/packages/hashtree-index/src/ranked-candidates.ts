import { type CID } from '@hashtree/core';
import { BTree } from './btree.js';
import { decodePosting, decodeTermStats } from './ranked-schema.js';
import type {
  ParsedRankedQuery,
  RankedPosting,
  RankedTermStats,
} from './ranked-types.js';

const POSTING_SEPARATOR = '\0';
const READ_BATCH_SIZE = 64;

export type RankedCandidate = {
  postings: Map<string, RankedPosting>;
};

export async function loadSelectedFrequencies(
  btree: BTree,
  termsRoot: CID,
  terms: readonly string[],
  selectedFields: ReadonlySet<string>,
  configuredFields: ReadonlyMap<string, unknown>,
  documentCount: number,
): Promise<Map<string, number>> {
  const frequencies = new Map<string, number>();
  const rawStats = await Promise.all(terms.map((term) => btree.get(termsRoot, term)));
  for (let index = 0; index < terms.length; index += 1) {
    const raw = rawStats[index];
    if (!raw) continue;
    const term = terms[index];
    const stats = decodeTermStats(raw);
    validateTermFields(stats, configuredFields, term);
    if (stats.documentFrequency > documentCount) {
      throw new Error(`Invalid ranked search document frequency: ${term}`);
    }
    const frequency = stats.fieldSets
      .filter((fieldSet) => fieldSet.fields.some((field) => selectedFields.has(field)))
      .reduce((total, fieldSet) => total + fieldSet.documentFrequency, 0);
    if (frequency > documentCount) {
      throw new Error(`Invalid ranked search document frequency: ${term}`);
    }
    if (frequency > 0) frequencies.set(term, frequency);
  }
  return frequencies;
}

export function hasMissingRequiredTerm(
  parsed: ParsedRankedQuery,
  frequencies: ReadonlyMap<string, number>,
  operator: 'or' | 'and',
): boolean {
  if (operator === 'and' && parsed.terms.some((term) => !frequencies.has(term))) return true;
  return parsed.phrases.some((phrase) => phrase.some((term) => !frequencies.has(term)));
}

export async function collectRankedCandidates(
  btree: BTree,
  postingsRoot: CID,
  terms: readonly string[],
  frequencies: ReadonlyMap<string, number>,
  fields: ReadonlySet<string>,
  operator: 'or' | 'and',
): Promise<Map<string, RankedCandidate>> {
  return operator === 'and'
    ? await collectAndCandidates(btree, postingsRoot, terms, frequencies, fields)
    : await collectOrCandidates(btree, postingsRoot, terms, frequencies, fields);
}

async function collectOrCandidates(
  btree: BTree,
  postingsRoot: CID,
  terms: readonly string[],
  frequencies: ReadonlyMap<string, number>,
  fields: ReadonlySet<string>,
): Promise<Map<string, RankedCandidate>> {
  const candidates = new Map<string, RankedCandidate>();
  for (const term of terms) {
    if (!frequencies.has(term)) continue;
    const prefix = `${term}${POSTING_SEPARATOR}`;
    for await (const [key, rawPosting] of btree.prefix(postingsRoot, prefix)) {
      const posting = decodePosting(rawPosting);
      if (!postingMatchesFields(posting, fields)) continue;
      const id = key.slice(prefix.length);
      const candidate = candidates.get(id) ?? { postings: new Map() };
      candidate.postings.set(term, posting);
      candidates.set(id, candidate);
    }
  }
  return candidates;
}

async function collectAndCandidates(
  btree: BTree,
  postingsRoot: CID,
  terms: readonly string[],
  frequencies: ReadonlyMap<string, number>,
  fields: ReadonlySet<string>,
): Promise<Map<string, RankedCandidate>> {
  const rankedTerms = [...terms].sort((left, right) =>
    (frequencies.get(left) ?? 0) - (frequencies.get(right) ?? 0)
      || compareStrings(left, right));
  const candidates = await collectOrCandidates(
    btree,
    postingsRoot,
    rankedTerms.slice(0, 1),
    frequencies,
    fields,
  );

  for (const term of rankedTerms.slice(1)) {
    const ids = [...candidates.keys()];
    for (let offset = 0; offset < ids.length; offset += READ_BATCH_SIZE) {
      const batch = ids.slice(offset, offset + READ_BATCH_SIZE);
      const postings = await Promise.all(batch.map((id) =>
        btree.get(postingsRoot, postingKey(term, id))));
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
    if (candidates.size === 0) break;
  }
  return candidates;
}

function validateTermFields(
  stats: RankedTermStats,
  fields: ReadonlyMap<string, unknown>,
  term: string,
): void {
  for (const fieldSet of stats.fieldSets) {
    for (const field of fieldSet.fields) {
      if (!fields.has(field)) {
        throw new Error(`Unknown ranked search term field for ${term}: ${field}`);
      }
    }
  }
}

function postingMatchesFields(posting: RankedPosting, fields: ReadonlySet<string>): boolean {
  return Object.keys(posting.fields).some((field) => fields.has(field));
}

function postingKey(term: string, id: string): string {
  return `${term}${POSTING_SEPARATOR}${id}`;
}

function compareStrings(left: string, right: string): number {
  return left < right ? -1 : left > right ? 1 : 0;
}
