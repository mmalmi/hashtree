import type {
  RankedSearchFieldManifest,
  RankedSearchScoringContext,
  RankedSearchSegmentManifest,
  RankedTermStats,
} from './ranked-types.js';

export interface PreparedRankedScoringContext {
  corpusDocuments: number;
  k1: number;
  fields: ReadonlyMap<string, RankedSearchFieldManifest>;
  frequencies: ReadonlyMap<string, number>;
}

export function prepareRankedScoringContext(
  context: RankedSearchScoringContext,
  segment: RankedSearchSegmentManifest,
  queryTerms: readonly string[],
  selectedFields: ReadonlySet<string>,
  localFrequencies: ReadonlyMap<string, number>,
): PreparedRankedScoringContext {
  const corpus = context?.corpus;
  if (!corpus || !nonNegativeSafeInteger(corpus.documentCount)
    || corpus.documentCount < segment.documentCount) {
    throw new Error('Invalid global ranked corpus document count');
  }
  if (!positiveFiniteNumber(corpus.k1) || corpus.k1 !== segment.k1) {
    throw new Error('Invalid global ranked corpus k1');
  }
  if (!Array.isArray(corpus.fields) || corpus.fields.length !== segment.fields.length) {
    throw new Error('Invalid global ranked corpus fields');
  }

  const fields = new Map<string, RankedSearchFieldManifest>();
  for (const rawField of corpus.fields) {
    const local = segment.fields.find((field) => field.name === rawField?.name);
    if (!local || fields.has(rawField.name)
      || rawField.boost !== local.boost
      || rawField.lengthNormalization !== local.lengthNormalization) {
      throw new Error(`Invalid global ranked corpus field configuration: ${String(rawField?.name)}`);
    }
    if (!nonNegativeSafeInteger(rawField.totalLength)
      || !nonNegativeSafeInteger(rawField.populatedDocumentCount)
      || rawField.totalLength < rawField.populatedDocumentCount
      || rawField.totalLength < local.totalLength
      || rawField.populatedDocumentCount < local.populatedDocumentCount
      || rawField.populatedDocumentCount > corpus.documentCount) {
      throw new Error(`Invalid global ranked corpus field totals: ${rawField.name}`);
    }
    fields.set(rawField.name, { ...rawField });
  }
  if (segment.fields.some((field) => !fields.has(field.name))) {
    throw new Error('Invalid global ranked corpus fields');
  }
  if (!context.termStatistics || typeof context.termStatistics.get !== 'function') {
    throw new Error('Invalid global ranked term statistics map');
  }

  const frequencies = new Map<string, number>();
  for (const term of queryTerms) {
    const rawStatistics = context.termStatistics.get(term);
    if (rawStatistics === undefined) continue;
    const statistics = validateTermStatistics(rawStatistics, fields, corpus.documentCount, term);
    const frequency = statistics.fieldSets
      .filter((fieldSet) => fieldSet.fields.some((field) => selectedFields.has(field)))
      .reduce((sum, fieldSet) => sum + fieldSet.documentFrequency, 0);
    if (frequency > 0) frequencies.set(term, frequency);
  }

  for (const [term, localFrequency] of localFrequencies) {
    const globalFrequency = frequencies.get(term);
    if (globalFrequency === undefined) {
      throw new Error(`Missing global ranked term statistics: ${term}`);
    }
    if (globalFrequency < localFrequency) {
      throw new Error(`Invalid global ranked term statistics: ${term}`);
    }
  }

  return {
    corpusDocuments: corpus.documentCount,
    k1: corpus.k1,
    fields,
    frequencies,
  };
}

function validateTermStatistics(
  value: RankedTermStats,
  fields: ReadonlyMap<string, RankedSearchFieldManifest>,
  documentCount: number,
  term: string,
): RankedTermStats {
  if (!value || !positiveSafeInteger(value.documentFrequency)
    || value.documentFrequency > documentCount
    || !Array.isArray(value.fieldSets)
    || value.fieldSets.length === 0) {
    throw new Error(`Invalid global ranked term statistics: ${term}`);
  }
  const seen = new Set<string>();
  let total = 0;
  for (const fieldSet of value.fieldSets) {
    if (!fieldSet || !Array.isArray(fieldSet.fields) || fieldSet.fields.length === 0
      || !positiveSafeInteger(fieldSet.documentFrequency)) {
      throw new Error(`Invalid global ranked term statistics: ${term}`);
    }
    for (let index = 0; index < fieldSet.fields.length; index += 1) {
      const field = fieldSet.fields[index];
      if (typeof field !== 'string' || !fields.has(field)
        || (index > 0 && field <= fieldSet.fields[index - 1])) {
        throw new Error(`Invalid global ranked term statistics: ${term}`);
      }
    }
    const key = JSON.stringify(fieldSet.fields);
    if (seen.has(key)) {
      throw new Error(`Invalid global ranked term statistics: ${term}`);
    }
    seen.add(key);
    total += fieldSet.documentFrequency;
    if (!Number.isSafeInteger(total)) {
      throw new Error(`Invalid global ranked term statistics: ${term}`);
    }
  }
  if (total !== value.documentFrequency) {
    throw new Error(`Invalid global ranked term statistics: ${term}`);
  }
  return value;
}

function nonNegativeSafeInteger(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0;
}

function positiveSafeInteger(value: unknown): value is number {
  return nonNegativeSafeInteger(value) && value > 0;
}

function positiveFiniteNumber(value: unknown): value is number {
  return typeof value === 'number' && Number.isFinite(value) && value > 0;
}
