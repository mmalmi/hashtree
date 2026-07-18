import type { NostrEventQuery, ListEventsOptions } from '@hashtree/nostr';
import type { NostrFilter } from './nostrTypes.js';
import {
  HashtreeNostrFilterError,
  HashtreeNostrUnsupportedSearchError,
} from './types.js';

const HEX_64 = /^[0-9a-f]{64}$/;

export interface PlannedFilterQuery {
  query: NostrEventQuery;
  options: ListEventsOptions;
  impossible: boolean;
}

export function validateHashtreeNostrFilters(filters: readonly NostrFilter[]): void {
  for (const filter of filters) {
    if (Object.prototype.hasOwnProperty.call(filter, 'search') && filter.search !== undefined) {
      throw new HashtreeNostrUnsupportedSearchError();
    }
    validateHexValues(filter.ids, 'event id');
    validateHexValues(filter.authors, 'author');
    validateKinds(filter.kinds);
    validateInteger(filter.since, 'since');
    validateInteger(filter.until, 'until');
    validateInteger(filter.limit, 'limit');

    for (const [key, values] of Object.entries(filter)) {
      if (!key.startsWith('#')) continue;
      if (!Array.isArray(values) || values.length === 0 || values.some((value) => typeof value !== 'string')) {
        throw new HashtreeNostrFilterError(`${key} must be a non-empty array of strings`);
      }
      const stringValues = values as string[];
      if ((key === '#e' || key === '#p') && stringValues.some((value) => !HEX_64.test(value))) {
        throw new HashtreeNostrFilterError(`${key} filters must contain exact lowercase 64-character hex values`);
      }
    }
  }
}

export function planFilterQuery(
  filter: NostrFilter,
  globalLimit: number | undefined,
): PlannedFilterQuery {
  const tags: Record<string, readonly string[]> = {};

  for (const [key, values] of Object.entries(filter)) {
    if (key.startsWith('#') && Array.isArray(values)) {
      tags[key.slice(1)] = values as string[];
    }
  }

  const candidateLimit = minimumDefined(globalLimit, filter.limit);

  return {
    query: {
      ids: filter.ids,
      authors: filter.authors,
      kinds: filter.kinds,
      tags: Object.keys(tags).length > 0 ? tags : undefined,
    },
    options: {
      since: filter.since,
      until: filter.until,
      limit: candidateLimit,
      strict: true,
    },
    impossible: filter.limit === 0
      || filter.since !== undefined && filter.until !== undefined && filter.since > filter.until,
  };
}

export function normalizeGlobalLimit(limit: number | undefined): number | undefined {
  if (limit === undefined) return undefined;
  validateInteger(limit, 'query limit');
  return limit;
}

function validateHexValues(values: readonly string[] | undefined, label: string): void {
  if (values === undefined) return;
  if (!Array.isArray(values) || values.length === 0 || values.some((value) => typeof value !== 'string' || !HEX_64.test(value))) {
    throw new HashtreeNostrFilterError(`${label} filters must contain exact lowercase 64-character hex values`);
  }
}

function validateKinds(values: readonly number[] | undefined): void {
  if (values === undefined) return;
  if (!Array.isArray(values) || values.length === 0 || values.some((value) => !Number.isSafeInteger(value) || value < 0 || value > 65_535)) {
    throw new HashtreeNostrFilterError('kind filters must be a non-empty array of integers from 0 through 65535');
  }
}

function validateInteger(value: number | undefined, label: string): void {
  if (value !== undefined && (!Number.isSafeInteger(value) || value < 0)) {
    throw new HashtreeNostrFilterError(`${label} must be a non-negative safe integer`);
  }
}

function minimumDefined(left: number | undefined, right: number | undefined): number | undefined {
  if (left === undefined) return right;
  if (right === undefined) return left;
  return Math.min(left, right);
}
