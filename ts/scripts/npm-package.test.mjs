import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import test from 'node:test';
import { npmManifest, isPublished, selectPackages } from './npm-package.mjs';

test('package selection includes local dependencies in release order and rejects typos', () => {
  const manifests = [
    { name: '@hashtree/core' },
    { name: '@hashtree/dexie', dependencies: { '@hashtree/core': 'release-url' } },
    { name: '@hashtree/worker', dependencies: { '@hashtree/dexie': 'release-url' } },
    { name: '@hashtree/fips-transport' },
  ];
  assert.deepEqual(selectPackages(manifests, '@hashtree/worker').map(({ name }) => name),
    ['@hashtree/core', '@hashtree/dexie', '@hashtree/worker']);
  assert.deepEqual(selectPackages(manifests, 'all'), manifests);
  assert.throws(() => selectPackages(manifests, '@hashtree/typo'), /Unknown package/);
  assert.throws(() => selectPackages(manifests, ''), /Select at least one/);
});

test('npm archive metadata uses registry versions and the provenance repository', () => {
  const original = {
    name: '@hashtree/worker', version: '0.4.3',
    dependencies: { '@hashtree/core': 'https://example.invalid/core.tgz', dexie: '4.4.2' },
    peerDependencies: { ndk: '*' },
  };
  const result = npmManifest(original, 'hashtree-worker', new Map([['@hashtree/core', '0.3.2']]));
  assert.equal(result.dependencies['@hashtree/core'], '0.3.2');
  assert.equal(result.dependencies.dexie, '4.4.2');
  assert.deepEqual(result.peerDependencies, { ndk: '*' });
  assert.deepEqual(result.repository, {
    type: 'git', url: 'git+https://github.com/mmalmi/hashtree.git',
    directory: 'ts/packages/hashtree-worker',
  });
  assert.equal(original.dependencies['@hashtree/core'], 'https://example.invalid/core.tgz');
});

test('npm archives reject unresolved workspace and local runtime dependencies', () => {
  for (const specifier of ['workspace:*', 'link:../core', 'file:../core']) {
    assert.throws(() => npmManifest({ dependencies: { unknown: specifier } }, 'example', new Map()), /local dependency/);
  }
});

test('registry lookup skips only existing versions and fails closed on registry errors', async () => {
  const server = createServer((request, response) => {
    const status = request.url.includes('published') ? 200 : request.url.includes('missing') ? 404 : 503;
    response.writeHead(status, { 'content-type': 'application/json' });
    response.end(JSON.stringify(status === 200 ? { name: '@hashtree/published', version: '1.0.0' } : { error: 'test' }));
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const registry = `http://127.0.0.1:${server.address().port}`;
  try {
    assert.equal(await isPublished('@hashtree/published', '1.0.0', registry), true);
    assert.equal(await isPublished('@hashtree/missing', '1.0.0', registry), false);
    await assert.rejects(isPublished('@hashtree/error', '1.0.0', registry), /503/);
  } finally {
    await new Promise((resolve) => server.close(resolve));
  }
});
