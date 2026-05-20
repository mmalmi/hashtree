function isRecord(value) {
    return !!value && typeof value === 'object' && !Array.isArray(value);
}
function materializeDefaults(schema) {
    if (!schema.defaults) {
        return undefined;
    }
    return typeof schema.defaults === 'function'
        ? schema.defaults()
        : schema.defaults;
}
function applyDefaults(value, defaults) {
    if (!defaults) {
        return value;
    }
    if (value === undefined) {
        return defaults;
    }
    if (isRecord(value) && isRecord(defaults)) {
        return {
            ...defaults,
            ...value,
        };
    }
    return value;
}
export function getCollectionSchema(definition) {
    return definition.schema ?? null;
}
export function getSchemaVersion(definition) {
    return definition.schema?.version ?? definition.schemaVersion ?? 1;
}
export function normalizeCollectionItem(definition, value, options = {}) {
    const schema = getCollectionSchema(definition);
    if (!schema) {
        return value;
    }
    const fromVersion = options.fromVersion ?? schema.version;
    let next = value;
    if (fromVersion !== schema.version) {
        if (!schema.migrate) {
            throw new Error(`Collection schema migration required: ${fromVersion} -> ${schema.version}`);
        }
        next = schema.migrate(value, fromVersion);
    }
    next = applyDefaults(next, materializeDefaults(schema));
    if (schema.normalize) {
        next = schema.normalize(next);
    }
    if (schema.validate) {
        schema.validate(next);
    }
    return next;
}
//# sourceMappingURL=schema.js.map