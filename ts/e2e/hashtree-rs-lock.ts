import fs from 'fs';

const LOCK_PATH = '/tmp/hashtree-rs-e2e.lock';

export async function acquireHashtreeRsLock(timeoutMs = 60000): Promise<number> {
  const start = Date.now();
  while (true) {
    try {
      return fs.openSync(LOCK_PATH, 'wx');
    } catch (err) {
      const code = (err as NodeJS.ErrnoException).code;
      if (code !== 'EEXIST') throw err;
      try {
        const stat = fs.statSync(LOCK_PATH);
        if (Date.now() - stat.mtimeMs > timeoutMs) {
          fs.unlinkSync(LOCK_PATH);
          continue;
        }
      } catch {
        // If we can't stat/remove, fall through to wait and retry.
      }
      if (Date.now() - start > timeoutMs) {
        throw new Error('Timed out waiting for hashtree-rs lock');
      }
      await new Promise(r => setTimeout(r, 500));
    }
  }
}

export function releaseHashtreeRsLock(fd: number): void {
  try {
    fs.closeSync(fd);
  } catch {
    // ignore
  }
  try {
    fs.unlinkSync(LOCK_PATH);
  } catch {
    // ignore
  }
}
