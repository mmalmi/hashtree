import { SearchIndex } from '@hashtree/index';
import type {
  CollectionEntryContext,
  CollectionDefinition,
  CollectionKeyIndexDefinition,
  CollectionSearchEntry,
  CollectionSearchIndexDefinition,
  CollectionSearchIndexOptions,
  Store,
} from './types.js';

export interface MaterializedCollectionSearchEntry {
  text: string;
  id?: string;
  cid?: CollectionSearchEntry['cid'];
  prefix?: string;
}

export function getSchemaVersion<T>(definition: CollectionDefinition<T>): number {
  return definition.schemaVersion ?? 1;
}

export function defaultSearchPrefix(name: string): string {
  return `${name}:`;
}

export function createSearchIndex(store: Store, options?: CollectionSearchIndexOptions): SearchIndex {
  return new SearchIndex(store, {
    order: options?.order,
    minKeywordLength: options?.minKeywordLength,
    stopWords: options?.stopWords ? new Set(options.stopWords) : undefined,
  });
}

export function materializeSearchText<T>(definition: CollectionSearchIndexDefinition<T>, item: T): string {
  return normalizeStringInput(definition.text?.(item) ?? []);
}

export function materializeSearchTerms<T>(
  definition: CollectionSearchIndexDefinition<T>,
  searchIndex: SearchIndex,
  text: string,
): string[] {
  const rawTerms = definition.terms
    ? definition.terms(text, {
        parseKeywords: (value) => searchIndex.parseKeywords(value),
      })
    : searchIndex.parseKeywords(text);

  return uniqueStrings(readStringInput(rawTerms).map((term) => term.toLowerCase()));
}

export function materializeSearchEntries<T>(
  definition: CollectionSearchIndexDefinition<T>,
  item: T,
  context: CollectionEntryContext,
): MaterializedCollectionSearchEntry[] {
  if (definition.entries) {
    return normalizeSearchEntries(definition.entries(item, context));
  }

  const text = materializeSearchText(definition, item);
  if (!text) {
    return [];
  }

  return [{
    id: context.id,
    cid: context.cid,
    prefix: definition.prefix,
    text,
  }];
}

export function materializeKeyValues<T>(definition: CollectionKeyIndexDefinition<T>, item: T): string[] {
  return uniqueStrings(readStringInput(definition.keys(item)));
}

function normalizeSearchEntries(
  value: Iterable<CollectionSearchEntry> | CollectionSearchEntry | null | undefined,
): MaterializedCollectionSearchEntry[] {
  if (!value) {
    return [];
  }

  const entries = isIterable(value)
    ? [...value]
    : [value];

  return entries
    .map((entry) => ({
      ...entry,
      id: entry.id?.trim(),
      prefix: entry.prefix?.trim(),
      text: normalizeStringInput(entry.text),
    }))
    .filter((entry) => entry.text);
}

export function readStringInput(value: Iterable<string> | string): string[] {
  if (typeof value === 'string') {
    return value.trim() ? [value] : [];
  }

  const parts: string[] = [];
  for (const entry of value) {
    const normalized = `${entry ?? ''}`.trim();
    if (normalized) {
      parts.push(normalized);
    }
  }
  return parts;
}

export function normalizeStringInput(value: Iterable<string> | string): string {
  return readStringInput(value).join(' ');
}

export function uniqueStrings(values: string[]): string[] {
  return [...new Set(values)];
}

function isIterable<T>(value: Iterable<T> | T): value is Iterable<T> {
  if (!value || typeof value !== 'object') {
    return false;
  }

  return Symbol.iterator in value;
}
