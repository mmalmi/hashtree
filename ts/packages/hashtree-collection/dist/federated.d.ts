import type { Store } from '@hashtree/core';
import type { FederatedCollectionSource, FederatedSearchHit, FederatedSearchOptions } from './types.js';
export declare function federatedSearch(store: Store, sources: Iterable<FederatedCollectionSource>, indexName: string, query: string, options?: FederatedSearchOptions): Promise<FederatedSearchHit[]>;
//# sourceMappingURL=federated.d.ts.map