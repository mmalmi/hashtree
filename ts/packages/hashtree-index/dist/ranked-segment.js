import { LinkType, } from '@hashtree/core';
import { decodeRankedManifest } from './ranked-schema.js';
const encoder = new TextEncoder();
const decoder = new TextDecoder();
export async function writeRankedSegment(tree, manifest, roots) {
    const manifestFile = await tree.putFile(encoder.encode(JSON.stringify(manifest)));
    const entries = [
        { name: 'manifest.json', cid: manifestFile.cid, size: manifestFile.size, type: LinkType.File },
    ];
    for (const [name, cid] of Object.entries(roots).sort(([left], [right]) => compareStrings(left, right))) {
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
    const link = (name) => entries.find((entry) => entry.name === name)?.cid ?? null;
    return {
        manifest: decodeRankedManifest(decoder.decode(manifestBytes)),
        roots: {
            postings: link('postings'),
            terms: link('terms'),
            documents: link('documents'),
            values: link('values'),
        },
    };
}
function compareStrings(left, right) {
    return left < right ? -1 : left > right ? 1 : 0;
}
//# sourceMappingURL=ranked-segment.js.map