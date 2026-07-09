import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { execFileSync, execSync } from 'child_process';
import { mkdtempSync, rmSync, writeFileSync, readFileSync, existsSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';

const pkgDir = join(__dirname, '..');

describe('package tarball', () => {
  let tempDir: string;
  let tarball: string;

  beforeAll(() => {
    // Build first
    execSync('pnpm build', { cwd: pkgDir, stdio: 'pipe' });

    tempDir = mkdtempSync(join(tmpdir(), 'hashtree-test-'));

    // Create the same publishable tarball that pnpm would upload to the npm registry.
    const pack = JSON.parse(execFileSync('pnpm', ['pack', '--json', '--pack-destination', tempDir], {
      cwd: pkgDir,
      encoding: 'utf-8',
    }));
    tarball = pack.filename;

    if (!existsSync(tarball)) {
      throw new Error(`Tarball not found at ${tarball}`);
    }

    writeFileSync(
      join(tempDir, 'package.json'),
      JSON.stringify({ name: 'test', type: 'module' })
    );
    execFileSync('pnpm', ['add', tarball], { cwd: tempDir, stdio: 'pipe' });
  });

  afterAll(() => {
    if (tempDir) rmSync(tempDir, { recursive: true, force: true });
  });

  it('should export main entry point', async () => {
    const testFile = join(tempDir, 'test-main.mjs');
    writeFileSync(
      testFile,
      `
      import { HashTree, MemoryStore, toHex, fromHex } from '@hashtree/core';

      if (typeof HashTree !== 'function') throw new Error('HashTree not exported');
      if (typeof MemoryStore !== 'function') throw new Error('MemoryStore not exported');
      if (typeof toHex !== 'function') throw new Error('toHex not exported');
      if (typeof fromHex !== 'function') throw new Error('fromHex not exported');

      // Basic functionality test
      const store = new MemoryStore();
      const tree = new HashTree({ store });
      const hash = await tree.putBlob(new TextEncoder().encode('hello'));
      const value = await tree.getBlob(hash);
      if (new TextDecoder().decode(value) !== 'hello') throw new Error('putBlob/getBlob failed');

      console.log('main entry point OK');
    `
    );
    const output = execSync(`node ${testFile}`, { encoding: 'utf-8' });
    expect(output.trim()).toBe('main entry point OK');
  });

  it('should export worker protocol entry point', async () => {
    const testFile = join(tempDir, 'test-worker.mjs');
    writeFileSync(
      testFile,
      `
      import { generateRequestId } from '@hashtree/core/worker';
      if (typeof generateRequestId !== 'function') throw new Error('generateRequestId not exported');
      const id = generateRequestId();
      if (typeof id !== 'string') throw new Error('generateRequestId should return string');
      console.log('worker entry point OK');
    `
    );
    const output = execSync(`node ${testFile}`, { encoding: 'utf-8' });
    expect(output.trim()).toBe('worker entry point OK');
  });

  it('should have correct package.json metadata', () => {
    const pkgPath = join(tempDir, 'node_modules', '@hashtree', 'core', 'package.json');
    const pkg = JSON.parse(readFileSync(pkgPath, 'utf-8'));
    expect(pkg.name).toBe('@hashtree/core');
    expect(pkg.type).toBe('module');
    expect(pkg.exports['.']).toBeDefined();
    expect(pkg.exports['./worker']).toBeDefined();
  });
});
