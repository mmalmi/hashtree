import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { runtimePackageDirs } from './runtime-packages.mjs';

// Release URL dependencies do not express the local declaration build order.
for (const directory of runtimePackageDirs) {
  execFileSync('pnpm', ['build'], {
    cwd: fileURLToPath(new URL(`../packages/${directory}/`, import.meta.url)),
    stdio: 'inherit',
  });
}
