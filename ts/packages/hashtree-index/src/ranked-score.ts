import type {
  RankedDocumentStats,
  RankedPosting,
  RankedSearchFieldManifest,
} from './ranked-types.js';

export function scoreBm25fTerm(options: {
  posting: RankedPosting;
  document: RankedDocumentStats;
  fields: ReadonlyMap<string, RankedSearchFieldManifest>;
  selectedFields: ReadonlySet<string>;
  corpusDocuments: number;
  documentFrequency: number;
  k1: number;
}): number {
  let weightedFrequency = 0;

  for (const [fieldName, fieldPosting] of Object.entries(options.posting.fields)) {
    if (!options.selectedFields.has(fieldName)) continue;
    const field = options.fields.get(fieldName);
    if (!field) continue;
    const length = options.document.lengths[fieldName] ?? 0;
    const averageLength = field.populatedDocumentCount > 0
      ? field.totalLength / field.populatedDocumentCount
      : 0;
    const normalization = averageLength > 0
      ? (1 - field.lengthNormalization)
        + field.lengthNormalization * (length / averageLength)
      : 1;
    weightedFrequency += field.boost * fieldPosting.frequency / Math.max(normalization, 1e-12);
  }

  if (weightedFrequency <= 0) return 0;
  const idf = inverseDocumentFrequency(
    options.corpusDocuments,
    options.documentFrequency,
  );
  return idf
    * ((options.k1 + 1) * weightedFrequency)
    / (options.k1 + weightedFrequency);
}

export function countMatchedPhrases(
  phrases: readonly (readonly string[])[],
  postingsByTerm: ReadonlyMap<string, RankedPosting>,
  selectedFields: ReadonlySet<string>,
): number {
  let matched = 0;
  for (const phrase of phrases) {
    if (matchesPhrase(phrase, postingsByTerm, selectedFields)) {
      matched += 1;
    }
  }
  return matched;
}

function matchesPhrase(
  phrase: readonly string[],
  postingsByTerm: ReadonlyMap<string, RankedPosting>,
  selectedFields: ReadonlySet<string>,
): boolean {
  if (phrase.length === 0) return true;

  for (const fieldName of selectedFields) {
    const firstPositions = postingsByTerm.get(phrase[0])?.fields[fieldName]?.positions;
    if (!firstPositions) continue;
    const following = phrase.slice(1).map((term) =>
      new Set(postingsByTerm.get(term)?.fields[fieldName]?.positions ?? []));

    if (firstPositions.some((start) =>
      following.every((positions, offset) => positions.has(start + offset + 1)))) {
      return true;
    }
  }

  return false;
}

function inverseDocumentFrequency(documentCount: number, documentFrequency: number): number {
  const boundedFrequency = Math.max(0, Math.min(documentFrequency, documentCount));
  return Math.log(1 + (documentCount - boundedFrequency + 0.5) / (boundedFrequency + 0.5));
}
