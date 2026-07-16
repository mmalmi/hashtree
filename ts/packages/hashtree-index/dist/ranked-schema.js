export const RANKED_SEARCH_SEGMENT_FORMAT = 'hashtree/ranked-search-segment@1';
export function normalizeRankedBuildOptions(options) {
    const fields = Object.entries(options.fields)
        .sort(([left], [right]) => compareStrings(left, right))
        .map(([name, field]) => {
        if (!name)
            throw new Error('Ranked search field names must not be empty');
        return {
            name,
            boost: positiveNumber(field.boost ?? 1, `fields.${name}.boost`),
            lengthNormalization: boundedNumber(field.lengthNormalization ?? 0.75, 0, 1, `fields.${name}.lengthNormalization`),
        };
    });
    if (fields.length === 0) {
        throw new Error('Ranked search segments require at least one field');
    }
    return {
        fields,
        k1: positiveNumber(options.k1 ?? 1.2, 'k1'),
        maxTokensPerField: positiveInteger(options.maxTokensPerField ?? 4096, 'maxTokensPerField'),
    };
}
export function encodePosting(posting) {
    return JSON.stringify(posting);
}
export function decodePosting(raw) {
    const value = jsonObject(raw, 'posting');
    const rawFields = objectValue(value.fields, 'posting.fields');
    const fields = emptyRecord();
    if (Object.keys(rawFields).length === 0)
        throw new Error('Invalid empty posting.fields');
    for (const fieldName of Object.keys(rawFields).sort(compareStrings)) {
        const field = objectValue(rawFields[fieldName], `posting.fields.${fieldName}`);
        const frequency = positiveInteger(field.frequency, `posting.fields.${fieldName}.frequency`);
        if (!Array.isArray(field.positions)) {
            throw new Error(`Invalid posting.fields.${fieldName}.positions`);
        }
        const positions = field.positions.map((position, index) => nonNegativeInteger(position, `posting.fields.${fieldName}.positions[${index}]`));
        if (positions.length !== frequency || positions.some((position, index) => index > 0 && position <= positions[index - 1])) {
            throw new Error(`Invalid posting.fields.${fieldName}.positions`);
        }
        fields[fieldName] = { frequency, positions };
    }
    return { fields };
}
export function encodeDocumentStats(stats) {
    return JSON.stringify(stats);
}
export function decodeDocumentStats(raw) {
    const value = jsonObject(raw, 'document stats');
    const rawLengths = objectValue(value.lengths, 'document stats.lengths');
    const lengths = emptyRecord();
    for (const fieldName of Object.keys(rawLengths).sort(compareStrings)) {
        lengths[fieldName] = nonNegativeInteger(rawLengths[fieldName], `document stats.lengths.${fieldName}`);
    }
    return { lengths };
}
export function encodeTermStats(stats) {
    return JSON.stringify(stats);
}
export function decodeTermStats(raw) {
    const value = jsonObject(raw, 'term stats');
    const documentFrequency = positiveInteger(value.documentFrequency, 'term stats.documentFrequency');
    if (!Array.isArray(value.fieldSets) || value.fieldSets.length === 0) {
        throw new Error('Invalid term stats.fieldSets');
    }
    const fieldSets = value.fieldSets.map((rawFieldSet, index) => {
        const fieldSet = objectValue(rawFieldSet, `term stats.fieldSets[${index}]`);
        if (!Array.isArray(fieldSet.fields)
            || fieldSet.fields.length === 0
            || fieldSet.fields.some((field) => typeof field !== 'string' || !field)) {
            throw new Error(`Invalid term stats.fieldSets[${index}].fields`);
        }
        const fields = [...fieldSet.fields];
        if (new Set(fields).size !== fields.length
            || fields.some((field, fieldIndex) => fieldIndex > 0 && field <= fields[fieldIndex - 1])) {
            throw new Error(`Invalid term stats.fieldSets[${index}].fields`);
        }
        return {
            fields,
            documentFrequency: positiveInteger(fieldSet.documentFrequency, `term stats.fieldSets[${index}].documentFrequency`),
        };
    });
    const fieldSetKeys = fieldSets.map((fieldSet) => JSON.stringify(fieldSet.fields));
    if (new Set(fieldSetKeys).size !== fieldSetKeys.length) {
        throw new Error('Duplicate term stats field set');
    }
    const fieldSetFrequency = fieldSets.reduce((total, fieldSet) => total + fieldSet.documentFrequency, 0);
    if (fieldSetFrequency !== documentFrequency) {
        throw new Error('Inconsistent term stats document frequency');
    }
    return { documentFrequency, fieldSets };
}
export function decodeRankedManifest(raw) {
    const value = jsonObject(raw, 'ranked search manifest');
    if (value.format !== RANKED_SEARCH_SEGMENT_FORMAT) {
        throw new Error(`Unsupported ranked search segment format: ${String(value.format)}`);
    }
    if (value.normalization !== 'NFKC-lowercase@1') {
        throw new Error(`Unsupported ranked search normalization: ${String(value.normalization)}`);
    }
    if (!Array.isArray(value.fields) || value.fields.length === 0) {
        throw new Error('Invalid ranked search manifest fields');
    }
    const fields = value.fields.map((rawField, index) => decodeManifestField(rawField, index));
    if (new Set(fields.map((field) => field.name)).size !== fields.length) {
        throw new Error('Duplicate ranked search manifest field');
    }
    const manifest = {
        format: RANKED_SEARCH_SEGMENT_FORMAT,
        normalization: 'NFKC-lowercase@1',
        documentCount: nonNegativeInteger(value.documentCount, 'manifest.documentCount'),
        termCount: nonNegativeInteger(value.termCount, 'manifest.termCount'),
        postingCount: nonNegativeInteger(value.postingCount, 'manifest.postingCount'),
        storedValueCount: nonNegativeInteger(value.storedValueCount, 'manifest.storedValueCount'),
        k1: positiveNumber(value.k1, 'manifest.k1'),
        maxTokensPerField: positiveInteger(value.maxTokensPerField, 'manifest.maxTokensPerField'),
        fields,
    };
    if ((manifest.termCount === 0) !== (manifest.postingCount === 0)) {
        throw new Error('Inconsistent ranked search term and posting counts');
    }
    if (manifest.storedValueCount > manifest.documentCount) {
        throw new Error('Invalid ranked search stored value count');
    }
    for (const field of manifest.fields) {
        if (field.populatedDocumentCount > manifest.documentCount) {
            throw new Error(`Invalid ranked search populated count for field: ${field.name}`);
        }
    }
    return manifest;
}
function decodeManifestField(raw, index) {
    const field = objectValue(raw, `manifest.fields[${index}]`);
    if (typeof field.name !== 'string' || !field.name) {
        throw new Error(`Invalid manifest.fields[${index}].name`);
    }
    return {
        name: field.name,
        boost: positiveNumber(field.boost, `manifest.fields[${index}].boost`),
        lengthNormalization: boundedNumber(field.lengthNormalization, 0, 1, `manifest.fields[${index}].lengthNormalization`),
        totalLength: nonNegativeInteger(field.totalLength, `manifest.fields[${index}].totalLength`),
        populatedDocumentCount: nonNegativeInteger(field.populatedDocumentCount, `manifest.fields[${index}].populatedDocumentCount`),
    };
}
function jsonObject(raw, label) {
    try {
        return objectValue(JSON.parse(raw), label);
    }
    catch (error) {
        if (error instanceof SyntaxError)
            throw new Error(`Invalid ${label} JSON`);
        throw error;
    }
}
function objectValue(value, label) {
    if (!value || typeof value !== 'object' || Array.isArray(value)) {
        throw new Error(`Invalid ${label}`);
    }
    return value;
}
function positiveInteger(value, label) {
    const number = nonNegativeInteger(value, label);
    if (number === 0)
        throw new Error(`Invalid ${label}`);
    return number;
}
function nonNegativeInteger(value, label) {
    if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) {
        throw new Error(`Invalid ${label}`);
    }
    return value;
}
function positiveNumber(value, label) {
    const number = nonNegativeNumber(value, label);
    if (number === 0)
        throw new Error(`Invalid ${label}`);
    return number;
}
function nonNegativeNumber(value, label) {
    if (typeof value !== 'number' || !Number.isFinite(value) || value < 0) {
        throw new Error(`Invalid ${label}`);
    }
    return value;
}
function boundedNumber(value, min, max, label) {
    const number = nonNegativeNumber(value, label);
    if (number < min || number > max)
        throw new Error(`Invalid ${label}`);
    return number;
}
function compareStrings(left, right) {
    return left < right ? -1 : left > right ? 1 : 0;
}
function emptyRecord() {
    return Object.create(null);
}
//# sourceMappingURL=ranked-schema.js.map