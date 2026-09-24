export function selectPackages(manifests, selection = 'all') {
  if (selection === 'all') return manifests;
  const byName = new Map(manifests.map((manifest) => [manifest.name, manifest]));
  const selected = new Set();
  function include(name) {
    if (selected.has(name)) return;
    const manifest = byName.get(name);
    if (!manifest) throw new Error(`Unknown package: ${name}`);
    selected.add(name);
    for (const dependency of Object.keys({ ...manifest.dependencies, ...manifest.optionalDependencies })) {
      if (byName.has(dependency)) include(dependency);
    }
  }
  for (const name of selection.split(',').map((name) => name.trim()).filter(Boolean)) include(name);
  if (!selected.size) throw new Error('Select at least one package');
  return manifests.filter(({ name }) => selected.has(name));
}

/** Metadata transformations shared by packing and publishing. */
export function npmManifest(manifest, directory, versions) {
  const result = structuredClone(manifest);
  result.repository = {
    type: 'git',
    url: 'git+https://github.com/mmalmi/hashtree.git',
    directory: `ts/packages/${directory}`,
  };
  for (const field of ['dependencies', 'optionalDependencies']) {
    for (const [name, specifier] of Object.entries(result[field] ?? {})) {
      if (versions.has(name)) result[field][name] = versions.get(name);
      else if (/^(file|link|workspace):/.test(specifier)) {
        throw new Error(`${manifest.name} has a local dependency: ${name}=${specifier}`);
      }
    }
  }
  return result;
}

export async function isPublished(name, version, registry = 'https://registry.npmjs.org') {
  const response = await fetch(`${registry}/${encodeURIComponent(name)}/${encodeURIComponent(version)}`, {
    signal: AbortSignal.timeout(30_000),
  });
  if (response.status === 404) return false;
  if (!response.ok) throw new Error(`Registry lookup for ${name}@${version}: HTTP ${response.status}`);
  const manifest = await response.json();
  if (manifest.name !== name || manifest.version !== version) {
    throw new Error(`Unexpected registry response for ${name}@${version}`);
  }
  return true;
}
