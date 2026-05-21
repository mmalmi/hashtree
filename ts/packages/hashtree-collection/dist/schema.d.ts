import type { CollectionDefinition, CollectionSchema } from './types.js';
export declare function getCollectionSchema<T>(definition: CollectionDefinition<T>): CollectionSchema<T> | null;
export declare function getSchemaVersion<T>(definition: CollectionDefinition<T>): number;
export declare function normalizeCollectionItem<T>(definition: CollectionDefinition<T>, value: unknown, options?: {
    fromVersion?: number;
}): T;
//# sourceMappingURL=schema.d.ts.map