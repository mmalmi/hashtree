/**
 * Core wasm-git utilities - module loading, locking, filesystem operations
 */
import type { CID } from 'hashtree';
import { LinkType } from 'hashtree';
import { getTree } from '../../store';

// Module type from wasm-git
export interface WasmGitModule {
  FS: {
    mkdir(path: string): void;
    writeFile(path: string, data: Uint8Array | string): void;
    readdir(path: string): string[];
    stat(path: string): { mode: number };
    readFile(path: string, opts?: { encoding?: string }): Uint8Array | string;
    chdir(path: string): void;
    cwd(): string;
    filesystems: { MEMFS: unknown };
    mount(fs: unknown, opts: unknown, path: string): void;
    unmount(path: string): void;
  };
  callMain(args: string[]): void;
  callWithOutput(args: string[]): string;
}

let wasmGitModule: WasmGitModule | null = null;
let moduleLoadPromise: Promise<WasmGitModule> | null = null;

// Mutex for serializing access to wasm-git module (single-threaded wasm can't handle concurrent ops)
let wasmGitLock: Promise<void> = Promise.resolve();
let repoCounter = 0;

/**
 * Create a unique repo path in the wasm filesystem.
 * Date + counter + random avoids collisions if cleanup fails.
 */
export function createRepoPath(prefix: string = 'repo'): string {
  const counter = repoCounter++;
  const rand = Math.random().toString(36).slice(2, 8);
  return `/${prefix}_${Date.now()}_${counter}_${rand}`;
}

/**
 * Execute a function with exclusive access to wasm-git module
 */
export async function withWasmGitLock<T>(fn: () => Promise<T>): Promise<T> {
  // Wait for any previous operation to complete
  const prevLock = wasmGitLock;
  let resolveLock: () => void;
  wasmGitLock = new Promise(resolve => { resolveLock = resolve; });
  await prevLock;
  try {
    return await fn();
  } finally {
    resolveLock!();
  }
}

/**
 * Recursively remove a directory from the wasm filesystem
 */
export function rmRf(module: WasmGitModule, path: string): void {
  try {
    const entries = module.FS.readdir(path);
    for (const entry of entries) {
      if (entry === '.' || entry === '..') continue;
      const fullPath = `${path}/${entry}`;
      try {
        const stat = module.FS.stat(fullPath);
        const isDir = (stat.mode & 0o170000) === 0o040000;
        if (isDir) {
          rmRf(module, fullPath);
        } else {
          (module.FS as unknown as { unlink(path: string): void }).unlink(fullPath);
        }
      } catch {
        // Skip files we can't remove
      }
    }
    (module.FS as unknown as { rmdir(path: string): void }).rmdir(path);
  } catch {
    // Directory may not exist or already be removed
  }
}

/**
 * Load the wasm-git module (lazy, singleton)
 */
export async function loadWasmGit(): Promise<WasmGitModule> {
  if (wasmGitModule) return wasmGitModule;
  if (moduleLoadPromise) return moduleLoadPromise;

  moduleLoadPromise = (async () => {
    // Set up silent print functions BEFORE module creation
    // This prevents wasm-git from setting up its own print functions that log to console
    const capturedOutput = { current: null as string[] | null };
    const capturedError = { current: null as string[] | null };
    let quitStatus: number | null = null;

    // Configure wasm-git with silent print functions
    (globalThis as Record<string, unknown>).wasmGitModuleOverrides = {
      locateFile: (path: string) => {
        if (path.endsWith('.wasm')) {
          return '/lg2_async.wasm';
        }
        return path;
      },
      // Provide print/printErr to prevent wasm-git from creating its own console-logging versions
      print: (msg: string) => {
        if (capturedOutput.current !== null) {
          capturedOutput.current.push(msg);
        }
      },
      printErr: (msg: string) => {
        if (capturedError.current !== null) {
          capturedError.current.push(msg);
        }
      },
    };

    // Import from node_modules (Vite will handle bundling the JS)
    const { default: createModule } = await import('wasm-git');
    wasmGitModule = await createModule();

    // Add callWithOutput since wasm-git won't create it when we provide print/printErr
    const moduleAny = wasmGitModule as Record<string, unknown>;
    moduleAny.quit = (status: number) => {
      quitStatus = status;
    };
    moduleAny.callWithOutput = (args: string[]) => {
      capturedOutput.current = [];
      capturedError.current = [];
      quitStatus = null;
      wasmGitModule!.callMain(args);
      const ret = capturedOutput.current.join('\n');
      const err = capturedError.current.join('\n');
      capturedOutput.current = null;
      capturedError.current = null;
      if (!quitStatus) {
        return ret;
      } else {
        throw quitStatus + ': ' + err;
      }
    };

    return wasmGitModule!;
  })();

  return moduleLoadPromise;
}

/**
 * Copy hashtree directory contents to wasm-git filesystem
 */
export async function copyToWasmFS(
  module: WasmGitModule,
  cid: CID,
  destPath: string
): Promise<void> {
  const tree = getTree();
  const entries = await tree.listDirectory(cid);

  for (const entry of entries) {
    const entryPath = `${destPath}/${entry.name}`;

    if (entry.type === LinkType.Dir) {
      try {
        module.FS.mkdir(entryPath);
      } catch {
        // Directory may already exist
      }
      await copyToWasmFS(module, entry.cid, entryPath);
    } else {
      const data = await tree.readFile(entry.cid);
      if (data) {
        module.FS.writeFile(entryPath, data);
      }
    }
  }
}

/**
 * Copy only the .git directory to wasm-git filesystem
 * Much faster than copyToWasmFS for read-only git operations (log, branches, etc.)
 */
export async function copyGitDirToWasmFS(
  module: WasmGitModule,
  rootCid: CID,
  destPath: string
): Promise<void> {
  const tree = getTree();

  // Find .git directory
  const gitDirResult = await tree.resolvePath(rootCid, '.git');
  if (!gitDirResult || gitDirResult.type !== LinkType.Dir) {
    throw new Error('No .git directory found');
  }

  // Create .git directory
  const gitPath = `${destPath}/.git`;
  try {
    module.FS.mkdir(gitPath);
  } catch {
    // May already exist
  }

  // Copy .git contents recursively
  await copyToWasmFS(module, gitDirResult.cid, gitPath);
}

/**
 * Read .git directory and return all files
 */
export function readGitDirectory(
  module: WasmGitModule,
  path: string = '.git',
  prefix: string = '.git'
): Array<{ name: string; data: Uint8Array; isDir: boolean }> {
  const gitFiles: Array<{ name: string; data: Uint8Array; isDir: boolean }> = [];

  function readDir(dirPath: string, dirPrefix: string): void {
    const entries = module.FS.readdir(dirPath);
    for (const entry of entries) {
      if (entry === '.' || entry === '..') continue;

      const fullPath = `${dirPath}/${entry}`;
      const relativePath = dirPrefix ? `${dirPrefix}/${entry}` : entry;

      try {
        const stat = module.FS.stat(fullPath);
        const isDir = (stat.mode & 0o170000) === 0o040000;
        if (isDir) {
          gitFiles.push({ name: relativePath, data: new Uint8Array(0), isDir: true });
          readDir(fullPath, relativePath);
        } else {
          const data = module.FS.readFile(fullPath) as Uint8Array;
          gitFiles.push({ name: relativePath, data, isDir: false });
        }
      } catch {
        // Skip files we can't read
      }
    }
  }

  readDir(path, prefix);
  return gitFiles;
}

/**
 * Run a git command silently (captures and discards output)
 * Use this for commands like init, add, commit that don't need output
 */
export function runSilent(module: WasmGitModule, args: string[]): void {
  // Use callWithOutput to capture output instead of logging it
  module.callWithOutput(args);
}

/**
 * Parse command string into args, handling quoted strings
 */
export function parseCommandArgs(command: string): string[] {
  const args: string[] = [];
  let current = '';
  let inQuote = false;
  let quoteChar = '';

  for (let i = 0; i < command.length; i++) {
    const char = command[i];

    if (inQuote) {
      if (char === quoteChar) {
        inQuote = false;
      } else {
        current += char;
      }
    } else if (char === '"' || char === "'") {
      inQuote = true;
      quoteChar = char;
    } else if (char === ' ') {
      if (current) {
        args.push(current);
        current = '';
      }
    } else {
      current += char;
    }
  }

  if (current) {
    args.push(current);
  }

  return args;
}
