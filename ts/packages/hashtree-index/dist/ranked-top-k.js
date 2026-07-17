import { LinkType, } from '@hashtree/core';
import { scoreBm25fTerm } from './ranked-score.js';
export const RANKED_TOP_K_FORMAT = 'hashtree/ranked-top-k@1';
export const RANKED_TOP_K_BLOCK_SIZE = 32;
export const RANKED_TOP_K_FANOUT = 32;
export const RANKED_TOP_K_MIN_DOCUMENT_FREQUENCY = 128;
const encoder = new TextEncoder();
const decoder = new TextDecoder();
export async function buildRankedTopK(btree, tree, entriesByTerm, termStatistics, segment) {
    const roots = [];
    let blockCount = 0;
    const fields = new Map(segment.fields.map((field) => [field.name, field]));
    const selectedFields = new Set(fields.keys());
    for (const term of [...entriesByTerm.keys()].sort(compareStrings)) {
        const statistics = termStatistics.get(term);
        if (!statistics)
            throw new Error(`Missing ranked top-k term statistics: ${term}`);
        const sourceEntries = entriesByTerm.get(term) ?? [];
        if (sourceEntries.length !== statistics.documentFrequency) {
            throw new Error(`Inconsistent ranked top-k posting coverage: ${term}`);
        }
        if (sourceEntries.length < RANKED_TOP_K_MIN_DOCUMENT_FREQUENCY)
            continue;
        const entries = sourceEntries.map((entry) => ({
            entry,
            score: scoreBm25fTerm({
                posting: entry.posting,
                document: entry.document,
                fields,
                selectedFields,
                corpusDocuments: segment.documentCount,
                documentFrequency: statistics.documentFrequency,
                k1: segment.k1,
            }),
        })).sort((left, right) => right.score - left.score || compareStrings(left.entry.id, right.entry.id))
            .map(({ entry }) => entry);
        let level = [];
        for (let offset = 0; offset < entries.length; offset += RANKED_TOP_K_BLOCK_SIZE) {
            const block = entries.slice(offset, offset + RANKED_TOP_K_BLOCK_SIZE);
            level.push(await writeLeaf(tree, block));
            blockCount += 1;
        }
        while (level.length > 1) {
            const parents = [];
            for (let offset = 0; offset < level.length; offset += RANKED_TOP_K_FANOUT) {
                parents.push(await writeInternal(tree, level.slice(offset, offset + RANKED_TOP_K_FANOUT)));
            }
            level = parents;
        }
        if (level.length !== 1)
            throw new Error(`Missing ranked top-k postings: ${term}`);
        roots.push([term, level[0].cid]);
    }
    const postingCount = roots.reduce((count, [term]) => count + (termStatistics.get(term)?.documentFrequency ?? 0), 0);
    return {
        manifest: {
            format: RANKED_TOP_K_FORMAT,
            blockSize: RANKED_TOP_K_BLOCK_SIZE,
            fanout: RANKED_TOP_K_FANOUT,
            minimumDocumentFrequency: RANKED_TOP_K_MIN_DOCUMENT_FREQUENCY,
            termCount: roots.length,
            blockCount,
            postingCount,
        },
        roots: await btree.buildLinks(roots),
    };
}
export async function readRankedTopKNode(tree, root) {
    const entries = await tree.listDirectory(root);
    const metadata = entries.find((entry) => entry.name === 'node.json');
    if (!metadata || metadata.type !== LinkType.File) {
        throw new Error('Missing ranked top-k node metadata');
    }
    const bytes = await tree.readFile(metadata.cid);
    if (!bytes)
        throw new Error('Unreadable ranked top-k node metadata');
    const wire = decodeNode(decoder.decode(bytes));
    const links = entries
        .filter((entry) => entry.name !== 'node.json')
        .sort((left, right) => compareStrings(left.name, right.name));
    if (wire.ids) {
        if (wire.children || links.length > 0 || wire.ids.length !== wire.summary.count) {
            throw new Error('Invalid ranked top-k leaf');
        }
        if (wire.ids.length > RANKED_TOP_K_BLOCK_SIZE) {
            throw new Error('Oversized ranked top-k leaf');
        }
        if (new Set(wire.ids).size !== wire.ids.length
            || wire.ids.some((id) => typeof id !== 'string' || !id)) {
            throw new Error('Invalid ranked top-k leaf ids');
        }
        if ([...wire.ids].sort(compareStrings)[0] !== wire.summary.minId) {
            throw new Error('Inconsistent ranked top-k leaf id bound');
        }
        return { kind: 'leaf', summary: wire.summary, ids: wire.ids };
    }
    if (!wire.children || wire.children.length === 0 || links.length !== wire.children.length) {
        throw new Error('Invalid ranked top-k internal node');
    }
    if (wire.children.length > RANKED_TOP_K_FANOUT) {
        throw new Error('Oversized ranked top-k internal node');
    }
    const children = wire.children.map((child, index) => {
        const link = links[index];
        if (link.name !== child.name || link.type !== LinkType.Dir) {
            throw new Error('Invalid ranked top-k child link');
        }
        return { cid: link.cid, summary: child.summary };
    });
    if (!summariesEqual(wire.summary, mergeSummaries(children.map((child) => child.summary)))) {
        throw new Error('Inconsistent ranked top-k internal summary');
    }
    return { kind: 'internal', summary: wire.summary, children };
}
export function encodeRankedTopKManifest(manifest) {
    return JSON.stringify(manifest);
}
export function decodeRankedTopKManifest(raw) {
    const value = jsonObject(raw, 'ranked top-k manifest');
    if (value.format !== RANKED_TOP_K_FORMAT) {
        throw new Error(`Unsupported ranked top-k format: ${String(value.format)}`);
    }
    const manifest = {
        format: RANKED_TOP_K_FORMAT,
        blockSize: positiveInteger(value.blockSize, 'ranked top-k blockSize'),
        fanout: positiveInteger(value.fanout, 'ranked top-k fanout'),
        minimumDocumentFrequency: positiveInteger(value.minimumDocumentFrequency, 'ranked top-k minimumDocumentFrequency'),
        termCount: nonNegativeInteger(value.termCount, 'ranked top-k termCount'),
        blockCount: nonNegativeInteger(value.blockCount, 'ranked top-k blockCount'),
        postingCount: nonNegativeInteger(value.postingCount, 'ranked top-k postingCount'),
    };
    if (manifest.blockSize !== RANKED_TOP_K_BLOCK_SIZE
        || manifest.fanout !== RANKED_TOP_K_FANOUT
        || manifest.minimumDocumentFrequency !== RANKED_TOP_K_MIN_DOCUMENT_FREQUENCY) {
        throw new Error('Unsupported ranked top-k layout');
    }
    return manifest;
}
function summarize(entries) {
    const fields = emptyRecord();
    for (const entry of entries) {
        for (const [fieldName, posting] of Object.entries(entry.posting.fields)) {
            const length = entry.document.lengths[fieldName];
            if (length === undefined || length < posting.frequency) {
                throw new Error(`Invalid ranked top-k document field: ${entry.id}/${fieldName}`);
            }
            const bound = fields[fieldName];
            fields[fieldName] = bound
                ? {
                    maxFrequency: Math.max(bound.maxFrequency, posting.frequency),
                    minLength: Math.min(bound.minLength, length),
                }
                : { maxFrequency: posting.frequency, minLength: length };
        }
    }
    return {
        count: entries.length,
        minId: entries.map((entry) => entry.id).sort(compareStrings)[0],
        fields: sortedRecord(fields),
    };
}
function mergeSummaries(summaries) {
    const fields = emptyRecord();
    let count = 0;
    let minId;
    for (const summary of summaries) {
        count += summary.count;
        if (minId === undefined || summary.minId < minId)
            minId = summary.minId;
        for (const [fieldName, candidate] of Object.entries(summary.fields)) {
            const bound = fields[fieldName];
            fields[fieldName] = bound
                ? {
                    maxFrequency: Math.max(bound.maxFrequency, candidate.maxFrequency),
                    minLength: Math.min(bound.minLength, candidate.minLength),
                }
                : { ...candidate };
        }
    }
    if (minId === undefined)
        throw new Error('Missing ranked top-k summary id');
    return { count, minId, fields: sortedRecord(fields) };
}
async function writeLeaf(tree, entries) {
    const summary = summarize(entries);
    return {
        summary,
        cid: await writeNode(tree, {
            format: RANKED_TOP_K_FORMAT,
            summary,
            ids: entries.map((entry) => entry.id),
        }, []),
    };
}
async function writeInternal(tree, children) {
    const summary = mergeSummaries(children.map((child) => child.summary));
    const descriptors = children.map((child, index) => ({
        name: index.toString().padStart(8, '0'),
        summary: child.summary,
    }));
    const links = children.map((child, index) => ({
        name: descriptors[index].name,
        cid: child.cid,
        size: 0,
        type: LinkType.Dir,
    }));
    return {
        summary,
        cid: await writeNode(tree, {
            format: RANKED_TOP_K_FORMAT,
            summary,
            children: descriptors,
        }, links),
    };
}
async function writeNode(tree, node, children) {
    const metadata = await tree.putFile(encoder.encode(JSON.stringify(node)));
    return (await tree.putDirectory([
        ...children,
        { name: 'node.json', cid: metadata.cid, size: metadata.size, type: LinkType.File },
    ])).cid;
}
function decodeNode(raw) {
    const value = jsonObject(raw, 'ranked top-k node');
    if (value.format !== RANKED_TOP_K_FORMAT) {
        throw new Error(`Unsupported ranked top-k node format: ${String(value.format)}`);
    }
    const summary = decodeSummary(value.summary, 'ranked top-k node summary');
    const ids = value.ids === undefined ? undefined : decodeIds(value.ids);
    const children = value.children === undefined ? undefined : decodeChildren(value.children);
    if ((ids === undefined) === (children === undefined)) {
        throw new Error('Invalid ranked top-k node kind');
    }
    return { format: RANKED_TOP_K_FORMAT, summary, ...(ids ? { ids } : { children }) };
}
function decodeIds(value) {
    if (!Array.isArray(value) || value.length === 0
        || value.some((id) => typeof id !== 'string' || !id)) {
        throw new Error('Invalid ranked top-k node ids');
    }
    return [...value];
}
function decodeChildren(value) {
    if (!Array.isArray(value) || value.length === 0) {
        throw new Error('Invalid ranked top-k node children');
    }
    const children = value.map((rawChild, index) => {
        const child = objectValue(rawChild, `ranked top-k node children[${index}]`);
        if (typeof child.name !== 'string' || !child.name) {
            throw new Error(`Invalid ranked top-k child name: ${index}`);
        }
        return {
            name: child.name,
            summary: decodeSummary(child.summary, `ranked top-k child summary: ${index}`),
        };
    });
    if (new Set(children.map((child) => child.name)).size !== children.length
        || children.some((child, index) => index > 0 && child.name <= children[index - 1].name)) {
        throw new Error('Invalid ranked top-k child ordering');
    }
    return children;
}
function decodeSummary(value, label) {
    const summary = objectValue(value, label);
    const rawFields = objectValue(summary.fields, `${label}.fields`);
    const fields = emptyRecord();
    for (const fieldName of Object.keys(rawFields).sort(compareStrings)) {
        if (!fieldName)
            throw new Error(`Invalid ${label} field name`);
        const field = objectValue(rawFields[fieldName], `${label}.fields.${fieldName}`);
        fields[fieldName] = {
            maxFrequency: positiveInteger(field.maxFrequency, `${label}.${fieldName}.maxFrequency`),
            minLength: positiveInteger(field.minLength, `${label}.${fieldName}.minLength`),
        };
    }
    if (Object.keys(fields).length === 0)
        throw new Error(`Invalid ${label} fields`);
    if (typeof summary.minId !== 'string' || !summary.minId) {
        throw new Error(`Invalid ${label}.minId`);
    }
    return {
        count: positiveInteger(summary.count, `${label}.count`),
        minId: summary.minId,
        fields,
    };
}
function summariesEqual(left, right) {
    return JSON.stringify(left) === JSON.stringify(right);
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
    const integer = nonNegativeInteger(value, label);
    if (integer === 0)
        throw new Error(`Invalid ${label}`);
    return integer;
}
function nonNegativeInteger(value, label) {
    if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) {
        throw new Error(`Invalid ${label}`);
    }
    return value;
}
function sortedRecord(record) {
    return Object.fromEntries(Object.keys(record).sort(compareStrings).map((key) => [key, record[key]]));
}
function emptyRecord() {
    return Object.create(null);
}
function compareStrings(left, right) {
    return left < right ? -1 : left > right ? 1 : 0;
}
//# sourceMappingURL=ranked-top-k.js.map