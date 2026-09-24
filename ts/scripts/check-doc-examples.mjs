import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import ts from 'typescript';
import { runtimePackageDirs } from './runtime-packages.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const work = join(root, '..', 'work');
mkdirSync(work, { recursive: true });
const temporary = mkdtempSync(join(work, 'docs-examples-'));
const expected = [
  'Hello, hashtree!',
  'Hello\n1\n2',
  'Hello, streaming!',
  'Persistent data',
  'Growing a garden\n1',
];

try {
  const examples = [...readFileSync(join(root, 'GETTING_STARTED.md'), 'utf8')
    .matchAll(/```typescript\n([\s\S]*?)\n```/g)].map((match) => match[1]);
  assert.equal(examples.length, expected.length, 'Every guide example needs an expected result');
  examples.push(...[...readFileSync(join(root, 'README.md'), 'utf8')
    .matchAll(/```typescript\n([\s\S]*?)\n```/g)].map((match) => match[1]));
  expected.push('Hello, hashtree!');
  assert.equal(examples.length, expected.length, 'Every README example needs an expected result');

  writeFileSync(join(temporary, 'package.json'), '{"type":"module"}\n');
  mkdirSync(join(temporary, 'node_modules', '@hashtree'), { recursive: true });
  for (const directory of runtimePackageDirs) {
    const packageDir = join(root, 'packages', directory);
    const manifest = JSON.parse(readFileSync(join(packageDir, 'package.json'), 'utf8'));
    symlinkSync(packageDir, join(temporary, 'node_modules', manifest.name), 'junction');
  }
  const files = examples.map((example, index) => {
    const filename = join(temporary, `example-${index}.ts`);
    writeFileSync(filename, example);
    return filename;
  });
  const program = ts.createProgram(files, {
    target: ts.ScriptTarget.ES2022,
    module: ts.ModuleKind.NodeNext,
    moduleResolution: ts.ModuleResolutionKind.NodeNext,
    strict: true,
    skipLibCheck: true,
    types: [],
    outDir: join(temporary, 'compiled'),
    noEmitOnError: true,
  });
  const diagnostics = ts.getPreEmitDiagnostics(program);
  assert.equal(diagnostics.length, 0, ts.formatDiagnosticsWithColorAndContext(diagnostics, {
    getCanonicalFileName: (name) => name,
    getCurrentDirectory: () => root,
    getNewLine: () => '\n',
  }));
  assert.equal(program.emit().emitSkipped, false);
  const dexieRequire = createRequire(join(root, 'packages/hashtree-dexie/package.json'));
  const indexedDb = pathToFileURL(dexieRequire.resolve('fake-indexeddb/auto')).href;
  for (const [index, output] of expected.entries()) {
    const actual = execFileSync(process.execPath, [
      '--import', indexedDb, join(temporary, 'compiled', `example-${index}.js`),
    ], { encoding: 'utf8', timeout: 30_000 });
    assert.equal(actual.trim(), output, `Example ${index + 1} returned unexpected output`);
  }
  console.log(`Type-checked and ran ${examples.length} documentation examples.`);
} finally {
  rmSync(temporary, { recursive: true, force: true });
}
