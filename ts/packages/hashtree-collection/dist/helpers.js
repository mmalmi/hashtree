import { SearchIndex } from '@hashtree/index';
export function getSchemaVersion(definition) {
    return definition.schemaVersion ?? 1;
}
export function defaultSearchPrefix(name) {
    return `${name}:`;
}
export function createSearchIndex(store, options) {
    return new SearchIndex(store, {
        order: options?.order,
        minKeywordLength: options?.minKeywordLength,
        stopWords: options?.stopWords ? new Set(options.stopWords) : undefined,
    });
}
export function materializeSearchText(definition, item) {
    return normalizeStringInput(definition.text?.(item) ?? []);
}
export function materializeSearchTerms(definition, searchIndex, text) {
    const rawTerms = definition.terms
        ? definition.terms(text, {
            parseKeywords: (value) => searchIndex.parseKeywords(value),
        })
        : searchIndex.parseKeywords(text);
    return uniqueStrings(readStringInput(rawTerms).map((term) => term.toLowerCase()));
}
export function materializeSearchEntries(definition, item, context) {
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
export function materializeKeyValues(definition, item) {
    return uniqueStrings(readStringInput(definition.keys(item)));
}
function normalizeSearchEntries(value) {
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
export function readStringInput(value) {
    if (typeof value === 'string') {
        return value.trim() ? [value] : [];
    }
    const parts = [];
    for (const entry of value) {
        const normalized = `${entry ?? ''}`.trim();
        if (normalized) {
            parts.push(normalized);
        }
    }
    return parts;
}
export function normalizeStringInput(value) {
    return readStringInput(value).join(' ');
}
export function uniqueStrings(values) {
    return [...new Set(values)];
}
function isIterable(value) {
    if (!value || typeof value !== 'object') {
        return false;
    }
    return Symbol.iterator in value;
}
//# sourceMappingURL=helpers.js.map