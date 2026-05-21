export function resolveWorkerPublicAssetUrl(baseUrl, assetPath, options) {
    const normalizedAssetPath = assetPath.replace(/^\/+/, '');
    const normalizedBaseUrl = typeof baseUrl === 'string' && baseUrl.trim() ? baseUrl.trim() : '/';
    if (/^https?:\/\//i.test(normalizedBaseUrl)) {
        return new URL(normalizedAssetPath, normalizedBaseUrl).toString();
    }
    if (normalizedBaseUrl === '.' || normalizedBaseUrl === './') {
        return new URL(`../${normalizedAssetPath}`, options.importMetaUrl).toString();
    }
    const rootedBaseUrl = normalizedBaseUrl.startsWith('/') ? normalizedBaseUrl : `/${normalizedBaseUrl}`;
    const basePath = rootedBaseUrl.endsWith('/') ? rootedBaseUrl : `${rootedBaseUrl}/`;
    return new URL(`${basePath}${normalizedAssetPath}`, options.origin).toString();
}
//# sourceMappingURL=publicAssetUrl.js.map