import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import ts from 'typescript';
import { runtimePackageDirs } from './runtime-packages.mjs';
import { checkDocNetworkExamples } from './check-doc-network-examples.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const work = join(root, '..', 'work');
mkdirSync(work, { recursive: true });
const temporary = mkdtempSync(join(work, 'docs-examples-'));
const documents = [
  ['../README.md', ['Hello, hashtree!']],
  ['README.md', ['Hello, hashtree!']],
  ['GETTING_STARTED.md', ['Hello, hashtree!', 'Hello\n1\n2', 'Hello, streaming!',
    'Persistent data', 'Growing a garden\n1']],
  ['packages/hashtree/README.md', ['Hello', null]],
  ['packages/hashtree-collection/README.md', ['1\n0\n1\n0']],
  ['packages/hashtree-index/README.md', ['Garden\nbook:1 Garden\nbook:2 Orchard\nnull', 'one']],
  ['packages/hashtree-dexie/README.md', ['true']],
  ['packages/hashtree-mesh/README.md', ['From origin\ntrue']],
  ['packages/hashtree-git/README.md', ['htree://self/myrepo#private\nmyrepo\nprivate']],
  ['packages/hashtree-merge/README.md', ['updated\nedits\n2']],
  ['packages/hashtree-nostr/README.md', [null, 'Hello from Nostr\nHello from Nostr']],
  ['packages/hashtree-nostr-pubsub/README.md', ['Indexed note']],
];

try {
  const examples = documents.flatMap(([document, outputs]) => {
    const blocks = [...readFileSync(join(root, document), 'utf8')
      .matchAll(/```typescript\n([\s\S]*?)\n```/g)];
    assert.equal(blocks.length, outputs.length, `${document}: every example needs a check`);
    return blocks.map((match, index) => ({
      code: match[1], document, block: index + 1, expected: outputs[index],
    }));
  });

  writeFileSync(join(temporary, 'package.json'), '{"type":"module"}\n');
  mkdirSync(join(temporary, 'node_modules', '@hashtree'), { recursive: true });
  for (const directory of runtimePackageDirs) {
    const packageDir = join(root, 'packages', directory);
    const manifest = JSON.parse(readFileSync(join(packageDir, 'package.json'), 'utf8'));
    symlinkSync(packageDir, join(temporary, 'node_modules', manifest.name), 'junction');
  }
  symlinkSync(join(root, 'node_modules/nostr-tools'), join(temporary, 'node_modules/nostr-tools'), 'junction');
  const files = examples.map((example, index) => {
    const filename = join(temporary, `example-${index}.ts`);
    writeFileSync(filename, example.code);
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
  const networkExamples = {};
  for (const [index, example] of examples.entries()) {
    const filename = join(temporary, 'compiled', `example-${index}.js`);
    if (example.expected === null) {
      Object.assign(networkExamples, await import(pathToFileURL(filename).href));
      continue;
    }
    const actual = execFileSync(process.execPath, ['--import', indexedDb, filename], {
      encoding: 'utf8', timeout: 30_000,
    });
    assert.equal(actual.trim(), example.expected,
      `${example.document}, block ${example.block}: unexpected output`);
  }
  await checkDocNetworkExamples(networkExamples);
  console.log(`Type-checked and ran ${examples.length} documentation examples (including network recipes).`);
} finally {
  rmSync(temporary, { recursive: true, force: true });
}
