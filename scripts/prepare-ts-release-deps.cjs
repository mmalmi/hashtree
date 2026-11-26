// Run the one required, reviewed lifecycle after the frozen install verifies
// its archives. Supplying the verified binary prevents esbuild's npm fallback.
const assert = require('node:assert/strict');
const { createHash } = require('node:crypto');
const { execFileSync } = require('node:child_process');
const { readFileSync } = require('node:fs');
const { join, resolve } = require('node:path');

assert.equal(process.platform, 'linux');
assert.equal(process.arch, 'x64');
const directory = resolve(__dirname, '../ts/node_modules/.pnpm/esbuild@0.28.1/node_modules/esbuild');
const manifest = JSON.parse(readFileSync(join(directory, 'package.json'), 'utf8'));
assert.equal(manifest.version, '0.28.1');
const key = '@esbuild/linux-x64/bin/esbuild';
const binary = require.resolve(key, { paths: [directory] });
assert.equal(createHash('sha256').update(readFileSync(binary)).digest('hex'),
  manifest['esbuild.binaryHashes'][key], 'Locked esbuild binary differs');
execFileSync(process.execPath, [join(directory, 'install.js')], {
  env: { ...process.env, ESBUILD_BINARY_PATH: binary }, stdio: 'inherit',
});
assert.equal(execFileSync(binary, ['--version'], { encoding: 'utf8' }).trim(), manifest.version);
