import type { CID, HashTree } from '@hashtree/core';
import { BTree } from './btree.js';
import {
  encodeDocumentStats,
  encodePosting,
  encodeTermStats,
  normalizeRankedBuildOptions,
  RANKED_SEARCH_SEGMENT_FORMAT,
} from './ranked-schema.js';
import { writeRankedSegment } from './ranked-segment.js';
import { tokenizeRankedField } from './ranked-tokenize.js';
import type {
  RankedPosting,
  RankedSearchBuildOptions,
  RankedSearchDocument,
  RankedSearchSegmentManifest,
  RankedTermStats,
} from './ranked-types.js';

const POSTING_SEPARATOR = '\0';

type MutableTermStats = {
  documentFrequency: number;
  fieldSets: Map<string, RankedTermStats['fieldSets'][number]>;
};

export async function buildRankedSegment(
  btree: BTree,
  tree: HashTree,
  documents: Iterable<RankedSearchDocument>,
  options: RankedSearchBuildOptions,
): Promise<CID> {
  const config = normalizeRankedBuildOptions(options);
  const sortedDocuments = [...documents].sort((left, right) =>
    compareStrings(left.id, right.id));
  validateDocuments(sortedDocuments);

  const postings: Array<[string, string]> = [];
  const documentStats: Array<[string, string]> = [];
  const values: Array<[string, string]> = [];
  const termStatistics = new Map<string, MutableTermStats>();
  const fieldTotals = new Map(config.fields.map((field) => [field.name, 0]));
  const populatedFields = new Map(config.fields.map((field) => [field.name, 0]));

  for (const document of sortedDocuments) {
    const lengths = emptyRecord<number>();
    const documentPostings = new Map<string, RankedPosting>();

    for (const field of config.fields) {
      const tokenized = tokenizeRankedField(
        document.fields[field.name],
        config.maxTokensPerField,
      );
      lengths[field.name] = tokenized.length;
      fieldTotals.set(field.name, (fieldTotals.get(field.name) ?? 0) + tokenized.length);
      if (tokenized.length > 0) {
        populatedFields.set(field.name, (populatedFields.get(field.name) ?? 0) + 1);
      }
      addFieldPostings(
        documentPostings,
        field.name,
        tokenized.occurrences,
      );
    }

    documentStats.push([document.id, encodeDocumentStats({ lengths })]);
    if (document.value !== undefined) values.push([document.id, document.value]);
    for (const [term, posting] of documentPostings) {
      postings.push([`${term}${POSTING_SEPARATOR}${document.id}`, encodePosting(posting)]);
      addTermStatistics(termStatistics, term, posting);
    }
  }

  const manifest = buildManifest(
    config,
    sortedDocuments.length,
    termStatistics.size,
    postings.length,
    values.length,
    fieldTotals,
    populatedFields,
  );
  const termStats = [...termStatistics].map(([term, stats]) => [
    term,
    encodeTermStats({
      documentFrequency: stats.documentFrequency,
      fieldSets: [...stats.fieldSets.values()].sort((left, right) =>
        compareStrings(JSON.stringify(left.fields), JSON.stringify(right.fields))),
    }),
  ] as [string, string]);
  const [postingsRoot, termsRoot, documentsRoot, valuesRoot] = await Promise.all([
    btree.build(postings),
    btree.build(termStats),
    btree.build(documentStats),
    btree.build(values),
  ]);

  return await writeRankedSegment(tree, manifest, {
    postings: postingsRoot,
    terms: termsRoot,
    documents: documentsRoot,
    values: valuesRoot,
  });
}

function addTermStatistics(
  statistics: Map<string, MutableTermStats>,
  term: string,
  posting: RankedPosting,
): void {
  const fields = Object.keys(posting.fields).sort(compareStrings);
  const fieldSetKey = JSON.stringify(fields);
  const stats = statistics.get(term) ?? { documentFrequency: 0, fieldSets: new Map() };
  const fieldSet = stats.fieldSets.get(fieldSetKey) ?? { fields, documentFrequency: 0 };
  stats.documentFrequency += 1;
  fieldSet.documentFrequency += 1;
  stats.fieldSets.set(fieldSetKey, fieldSet);
  statistics.set(term, stats);
}

function addFieldPostings(
  postings: Map<string, RankedPosting>,
  fieldName: string,
  occurrences: readonly { term: string; position: number }[],
): void {
  for (const occurrence of occurrences) {
    const posting = postings.get(occurrence.term) ?? { fields: emptyRecord() };
    const field = posting.fields[fieldName] ?? { frequency: 0, positions: [] };
    field.frequency += 1;
    field.positions.push(occurrence.position);
    posting.fields[fieldName] = field;
    postings.set(occurrence.term, posting);
  }
}

function buildManifest(
  config: ReturnType<typeof normalizeRankedBuildOptions>,
  documentCount: number,
  termCount: number,
  postingCount: number,
  storedValueCount: number,
  totals: ReadonlyMap<string, number>,
  populated: ReadonlyMap<string, number>,
): RankedSearchSegmentManifest {
  return {
    format: RANKED_SEARCH_SEGMENT_FORMAT,
    normalization: 'NFKC-lowercase@1',
    documentCount,
    termCount,
    postingCount,
    storedValueCount,
    k1: config.k1,
    maxTokensPerField: config.maxTokensPerField,
    fields: config.fields.map((field) => {
      const totalLength = totals.get(field.name) ?? 0;
      const populatedDocumentCount = populated.get(field.name) ?? 0;
      return {
        ...field,
        totalLength,
        populatedDocumentCount,
      };
    }),
  };
}

function validateDocuments(documents: readonly RankedSearchDocument[]): void {
  let previousId: string | undefined;
  for (const document of documents) {
    if (!document.id) throw new Error('Ranked search document ids must not be empty');
    if (document.id === previousId) {
      throw new Error(`Duplicate ranked search document id: ${document.id}`);
    }
    if (document.value !== undefined && typeof document.value !== 'string') {
      throw new Error(`Invalid ranked search document value: ${document.id}`);
    }
    previousId = document.id;
  }
}

function compareStrings(left: string, right: string): number {
  return left < right ? -1 : left > right ? 1 : 0;
}

function emptyRecord<T>(): Record<string, T> {
  return Object.create(null) as Record<string, T>;
}
