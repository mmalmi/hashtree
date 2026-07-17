import { LinkType, } from '@hashtree/core';
import { decodeRankedManifest } from './ranked-schema.js';
import { decodeRankedTopKManifest, encodeRankedTopKManifest, } from './ranked-top-k.js';
const encoder = new TextEncoder();
const decoder = new TextDecoder();
export async function writeRankedSegment(tree, manifest, roots, topKManifest) {
    const manifestFile = await tree.putFile(encoder.encode(JSON.stringify(manifest)));
    const entries = [
        { name: 'manifest.json', cid: manifestFile.cid, size: manifestFile.size, type: LinkType.File },
    ];
    if (topKManifest) {
        const topKFile = await tree.putFile(encoder.encode(encodeRankedTopKManifest(topKManifest)));
        entries.push({ name: 'top-k.json', cid: topKFile.cid, size: topKFile.size, type: LinkType.File });
    }
    const linkedRoots = {
        documents: roots.documents,
        postings: roots.postings,
        terms: roots.terms,
        'top-k-roots': roots.topK,
        values: roots.values,
    };
    for (const [name, cid] of Object.entries(linkedRoots).sort(([left], [right]) => compareStrings(left, right))) {
        if (cid)
            entries.push({ name, cid, size: 0, type: LinkType.Dir });
    }
    entries.sort((left, right) => compareStrings(left.name, right.name));
    return (await tree.putDirectory(entries)).cid;
}
export async function readRankedSegment(tree, root) {
    const entries = await tree.listDirectory(root);
    const manifestEntry = entries.find((entry) => entry.name === 'manifest.json');
    if (!manifestEntry)
        throw new Error('Missing ranked search segment manifest');
    const manifestBytes = await tree.readFile(manifestEntry.cid);
    if (!manifestBytes)
        throw new Error('Unreadable ranked search segment manifest');
    const topKEntry = entries.find((entry) => entry.name === 'top-k.json');
    const topKBytes = topKEntry ? await tree.readFile(topKEntry.cid) : null;
    if (topKEntry && !topKBytes)
        throw new Error('Unreadable ranked top-k manifest');
    const link = (name) => entries.find((entry) => entry.name === name)?.cid ?? null;
    const topKRoot = link('top-k-roots');
    if ((topKBytes !== null) !== (topKRoot !== null)) {
        throw new Error('Incomplete ranked top-k acceleration');
    }
    return {
        manifest: decodeRankedManifest(decoder.decode(manifestBytes)),
        topKManifest: topKBytes ? decodeRankedTopKManifest(decoder.decode(topKBytes)) : null,
        roots: {
            postings: link('postings'),
            terms: link('terms'),
            documents: link('documents'),
            values: link('values'),
            topK: topKRoot,
        },
    };
}
function compareStrings(left, right) {
    return left < right ? -1 : left > right ? 1 : 0;
}
//# sourceMappingURL=ranked-segment.js.map