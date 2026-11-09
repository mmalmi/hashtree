import assert from 'node:assert/strict';
import { readFile, writeFile, mkdir, readdir, realpath } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import { Peer, until } from './process.mjs';
import { sha } from './durable.mjs';
const here = dirname(fileURLToPath(import.meta.url));
const admittedBytes = await readFile(process.env.FIPS_FILE_INTEROP_INPUTS);
assert.equal(sha(admittedBytes), process.env.FIPS_FILE_INTEROP_INPUTS_SHA256, 'root-reviewed admission binding required');
const admitted = JSON.parse(admittedBytes);
assert.equal(admitted.source, '8c7290e63bce040a318c941ff8e1a1dc9a1409e3');
assert.equal(admitted.rustCrateSha256, '839740e5f304745a22990aada37eeb5b6a472046fb30948aa8e08455ba50bf1f');
assert.equal(admitted.vectorsSha256, '39254c05fb2a2dc9bc4b6dd21e775f79c25f9da089685b62254f6afdde099cf9');
assert.equal(sha(await readFile(admitted.rustBinary)), admitted.rustBinarySha256);
assert.equal(sha(await readFile(admitted.vectors)), admitted.vectorsSha256);
assert.equal(admitted.nodeVersion, process.version);
assert.equal(sha(await readFile(admitted.installedLock)), admitted.installedLockSha256);
assert.equal(sha(await readFile(admitted.candidatePack)), admitted.candidatePackSha256);
const require = createRequire(import.meta.url);
const fixedVersions = { '@fips/core': '0.0.56', '@fips/transport-websocket': '0.0.14', '@fips/tcp': '0.2.4', '@fips/file-transfer': '0.1.1' };
for (const row of admitted.installedPackages) {
  let manifest;
  for (const directory of require.resolve.paths(row.name) ?? []) {
    try { manifest = await realpath(join(directory, row.name, 'package.json')); break; } catch {}
  }
  assert(manifest, `missing installed ${row.name}`);
  const bytes = await readFile(manifest); assert.equal(sha(bytes), row.manifestSha256);
  const pkg = JSON.parse(bytes); assert.equal(pkg.version, row.version);
  if (fixedVersions[row.name]) assert.equal(row.version, fixedVersions[row.name]);
  if (row.name === '@fips/file-transfer') {
    assert.equal(row.version, '0.1.1'); const root = dirname(manifest), found = [];
    async function walk(path, relative = '') {
      for (const entry of await readdir(path, { withFileTypes: true })) {
        if (entry.name === 'node_modules') continue;
        const name = relative + entry.name; assert(!entry.isSymbolicLink());
        if (entry.isDirectory()) await walk(join(path, entry.name), name + '/');
        else { assert(entry.isFile()); found.push(name); assert.equal(sha(await readFile(join(path, entry.name))), row.members[name]); }
      }
    }
    await walk(root); assert.deepEqual(found.sort(), Object.keys(row.members).sort());
  }
}
assert.deepEqual(admitted.installedPackages.map(r => r.name).sort(),
  ['@fips/core', '@fips/file-transfer', '@fips/tcp', '@fips/transport-websocket', 'ws'].sort());
const { seed } = await import('./seed.mjs');
const out = admitted.evidenceDirectory; await mkdir(out, { mode: 0o700 });
const scenarios = [], allChildren = [];
let currentSeed;
async function startPair(directory, generation) {
  const config = { seed: currentSeed.url, vectors: admitted.vectors };
  for (const role of ['rust', 'ts']) {
    await mkdir(join(directory, role), { mode: 0o700, recursive: true });
    await writeFile(join(directory, `${role}-${generation}.json`), JSON.stringify({ ...config, directory: join(directory, role),
      // Public fixture-only identities; never use real credentials here.
      testSecret: (role === 'rust' ? '11' : '22').repeat(32) }), { flag: 'wx', mode: 0o600 });
  }
  const rust = new Peer(admitted.rustBinary, [join(directory, `rust-${generation}.json`)], join(directory, `rust-${generation}.log`));
  const ts = new Peer(process.execPath, [join(here, 'ts-peer.mjs'), join(directory, `ts-${generation}.json`)], join(directory, `ts-${generation}.log`));
  allChildren.push(rust, ts); const [r, t] = await Promise.all([rust.ready(), ts.ready()]);
  assert.equal(r.vectors.sha256, admitted.vectorsSha256); assert.equal(t.vectors.sha256, admitted.vectorsSha256);
  assert.equal(r.vectors.controls, 8); assert.equal(t.vectors.controls, 8);
  return { rust, ts, r, t };
}
async function startTransfer(pair, rustSender, command, interrupt) {
  const sender = rustSender ? pair.rust : pair.ts, receiver = rustSender ? pair.ts : pair.rust;
  const remote = rustSender ? pair.t : pair.r;
  await sender.command({ ...command, op: 'offer', peer: remote.peer, peerNpub: remote.npub, interrupt });
  const source = rustSender ? pair.r : pair.t;
  await receiver.command({ ...command, op: 'receive', peer: source.peer, peerNpub: source.npub, interrupt });
}
async function stopPair(pair) { await Promise.all([pair.ts.stop(), pair.rust.stop()]); }
try {
  for (const rustSender of [true, false]) for (const interrupted of [false, true]) {
    const name = `${rustSender ? 'rust-to-ts' : 'ts-to-rust'}-${interrupted ? 'restart' : 'complete'}`;
    const directory = join(out, name); await mkdir(directory, { mode: 0o700 }); currentSeed = await seed();
    const command = { id: name, token: '33'.repeat(32), size: interrupted ? 2097275 : 400123, salt: rustSender ? 7 : 31 };
    let pair = await startPair(directory, 0), prefix = 0;
    const originalIdentities = [pair.r.peer, pair.t.peer];
    await startTransfer(pair, rustSender, command, interrupted);
    if (interrupted) {
      await until(() => { pair.ts.live(); pair.rust.live(); return pair.ts.events.some(e => e.event === 'gate'); }, 'controlled file interruption');
      if (rustSender) prefix = (await pair.ts.command({ op: 'snapshots', id: command.id })).retained;
      else prefix = await until(async () => (await pair.rust.command({ op: 'snapshots' })).find(r => r.id === command.id && r.bytes > 0)?.bytes, 'native partial receive');
      assert(prefix > 0 && prefix < command.size);
      await stopPair(pair);
      assert(pair.ts.events.some(e => e.event === 'paused' && e.id === command.id));
      assert(!pair.ts.events.some(e => ['failed', 'cancelled'].includes(e.event)));
      pair = await startPair(directory, 1); assert.deepEqual([pair.r.peer, pair.t.peer], originalIdentities);
      const native = (await pair.rust.command({ op: 'snapshots' })).find(r => r.id === command.id);
      assert.equal(native.status, 'paused');
      const state = await pair.ts.command({ op: 'snapshots', id: command.id });
      assert.equal((rustSender ? state.receive : state.send).status, 'claimed');
      const retained = rustSender ? state.retained : native.bytes;
      assert(retained >= prefix && retained < command.size); prefix = retained;
      await startTransfer(pair, rustSender, command, false);
    }
    await until(() => { pair.ts.live(); pair.rust.live(); return [pair.ts, pair.rust].every(p => p.events.some(e => e.event === 'completed' && e.id === command.id)); }, 'both application completions', 90000);
    if (interrupted) assert(pair.ts.events.some(e => e.event === 'accepted' && e.bytes === prefix), 'resume must acknowledge the saved offset');
    const receiver = rustSender ? pair.ts : pair.rust;
    const files = await receiver.command({ ...command, op: 'verify' });
    const native = (await pair.rust.command({ op: 'snapshots' })).find(r => r.id === command.id);
    assert.equal(native.status, 'completed');
    const state = await pair.ts.command({ op: 'snapshots', id: command.id });
    assert.equal((rustSender ? state.receive : state.send).status, 'completed');
    await stopPair(pair);
    pair = await startPair(directory, 2); assert.deepEqual([pair.r.peer, pair.t.peer], originalIdentities);
    assert.equal((await pair.rust.command({ op: 'snapshots' })).find(r => r.id === command.id).status, 'completed');
    const restored = await pair.ts.command({ op: 'snapshots', id: command.id });
    assert.equal((rustSender ? restored.receive : restored.send).status, 'completed');
    assert.deepEqual(await (rustSender ? pair.ts : pair.rust).command({ ...command, op: 'verify' }), files);
    await stopPair(pair); await currentSeed.stop(); const network = currentSeed.stats(); currentSeed = undefined;
    assert(network.ingressBytes > 0 && network.ingressBytes <= network.ingressLimitBytes && network.egressBytes > 0 && network.maxConcurrent <= 8);
    scenarios.push({ name, resumed: interrupted, durablePrefixBytes: prefix, files: files.map(({ modifiedNs, ...r }) => r),
      bothTerminalCompleted: true, identitiesStable: true, completedStateSurvivesRestart: true, outputUnchangedAfterRestart: true, network });
  }
  await writeFile(join(out, 'interop-receipt.json'), JSON.stringify({ passed: true, scenarios, source: admitted.source,
    inputSha256: sha(admittedBytes), rustCrateSha256: admitted.rustCrateSha256, rustBinarySha256: admitted.rustBinarySha256,
    candidatePackSha256: admitted.candidatePackSha256, vectorsSha256: admitted.vectorsSha256,
    scope: 'Actual mixed-language file-transfer over authenticated loopback FIPS; not browser/mobile/deployment or binary reproducibility proof' }, null, 2), { flag: 'wx', mode: 0o600 });
} finally {
  const stopped = await Promise.allSettled(allChildren.map(p => p.stop()));
  await currentSeed?.stop();
  assert(stopped.every(r => r.status === 'fulfilled'), 'fixture child cleanup failed');
}
