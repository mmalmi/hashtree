import { CollectionSource } from './source.js';
export async function federatedSearch(store, sources, indexName, query, options = {}) {
    const sourceList = [...sources];
    const limit = options.limit ?? 20;
    const perSourceLimit = options.perSourceLimit ?? Math.max(limit * 2, 20);
    const localResults = await Promise.all(sourceList.map(async (sourceInput) => {
        const source = new CollectionSource(store, sourceInput.manifest);
        const boost = sourceInput.boost ?? 1;
        const results = await source.search(indexName, query, {
            ...options,
            limit: perSourceLimit,
        });
        return results.map((result) => ({
            sourceId: sourceInput.manifest.sourceId,
            cid: result.cid,
            id: result.id,
            score: result.score,
            boost,
            weightedScore: result.score * boost,
        }));
    }));
    const merged = new Map();
    for (const resultSet of localResults) {
        for (const result of resultSet) {
            const hit = {
                sourceId: result.sourceId,
                cid: result.cid,
                score: result.score,
                boost: result.boost,
            };
            const existing = merged.get(result.id);
            if (!existing) {
                merged.set(result.id, {
                    id: result.id,
                    cid: result.cid,
                    score: result.weightedScore,
                    bestSourceId: result.sourceId,
                    sourceIds: [result.sourceId],
                    hits: [hit],
                    bestWeightedScore: result.weightedScore,
                });
                continue;
            }
            existing.score += result.weightedScore;
            if (!existing.sourceIds.includes(result.sourceId)) {
                existing.sourceIds.push(result.sourceId);
            }
            existing.hits.push(hit);
            if (result.weightedScore > existing.bestWeightedScore) {
                existing.bestWeightedScore = result.weightedScore;
                existing.bestSourceId = result.sourceId;
                existing.cid = result.cid;
            }
        }
    }
    return [...merged.values()]
        .sort((left, right) => {
        if (right.score !== left.score) {
            return right.score - left.score;
        }
        if (right.sourceIds.length !== left.sourceIds.length) {
            return right.sourceIds.length - left.sourceIds.length;
        }
        return left.id.localeCompare(right.id);
    })
        .slice(0, limit)
        .map(({ bestWeightedScore: _bestWeightedScore, ...result }) => result);
}
//# sourceMappingURL=federated.js.map