import { type CID, type HashTree } from '@hashtree/core';
import { BTree } from './btree.js';
import {
  collectRankedCandidates,
  hasMissingRequiredTerm,
  loadSelectedFrequencies,
} from './ranked-candidates.js';
import { scoreTopCandidates } from './ranked-ranking.js';
import { readRankedSegment } from './ranked-segment.js';
import { parseRankedQuery } from './ranked-tokenize.js';
import type {
  RankedSearchOptions,
  RankedSearchResult,
} from './ranked-types.js';

export async function queryRankedSegment(
  btree: BTree,
  tree: HashTree,
  root: CID,
  query: string,
  options: RankedSearchOptions,
): Promise<RankedSearchResult[]> {
  const limit = normalizeLimit(options.limit);
  if (limit === 0) return [];
  const parsed = parseRankedQuery(query);
  if (parsed.terms.length === 0) return [];

  const { manifest, roots } = await readRankedSegment(tree, root);
  if (manifest.documentCount === 0 || manifest.postingCount === 0) return [];
  if (!roots.documents) throw new Error('Missing ranked search document index');
  if (!roots.postings || !roots.terms) throw new Error('Missing ranked search postings index');
  if (manifest.storedValueCount > 0 && !roots.values) {
    throw new Error('Missing ranked search values index');
  }

  const fields = new Map(manifest.fields.map((field) => [field.name, field]));
  const selectedFields = selectFields(fields, options.fields);
  if (selectedFields.size === 0) return [];
  const operator = normalizeOperator(options.operator);
  const frequencies = await loadSelectedFrequencies(
    btree,
    roots.terms,
    parsed.terms,
    selectedFields,
    fields,
    manifest.documentCount,
  );
  if (hasMissingRequiredTerm(parsed, frequencies, operator)) return [];
  const candidates = await collectRankedCandidates(
    btree,
    roots.postings,
    parsed.terms,
    frequencies,
    selectedFields,
    operator,
  );
  if (candidates.size === 0) return [];

  const top = await scoreTopCandidates({
    btree,
    documentsRoot: roots.documents,
    candidates,
    parsed,
    frequencies,
    fields,
    selectedFields,
    manifest,
    limit,
  });
  return await Promise.all(top.map(async (result) => {
    const value = roots.values ? await btree.get(roots.values, result.id) : null;
    return { ...result, ...(value !== null ? { value } : {}) };
  }));
}

function selectFields<T>(
  available: ReadonlyMap<string, T>,
  requested: readonly string[] | undefined,
): Set<string> {
  if (!requested) return new Set(available.keys());
  const selected = new Set<string>();
  for (const field of requested) {
    if (!available.has(field)) throw new Error(`Unknown ranked search field: ${field}`);
    selected.add(field);
  }
  return selected;
}

function normalizeOperator(operator: RankedSearchOptions['operator']): 'or' | 'and' {
  if (operator === undefined) return 'or';
  if (operator !== 'or' && operator !== 'and') {
    throw new Error(`Invalid ranked search operator: ${String(operator)}`);
  }
  return operator;
}

function normalizeLimit(limit: number | undefined): number {
  if (limit === undefined) return 20;
  if (limit === Number.POSITIVE_INFINITY) return Number.POSITIVE_INFINITY;
  if (!Number.isFinite(limit)) throw new Error('Invalid ranked search limit');
  return Math.max(0, Math.floor(limit));
}
