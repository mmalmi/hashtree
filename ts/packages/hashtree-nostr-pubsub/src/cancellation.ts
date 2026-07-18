import type { HashtreeNostrQueryOptions } from './types.js';

export class QueryCancellation {
  readonly signal?: AbortSignal;
  readonly deadline?: number;

  constructor(options: HashtreeNostrQueryOptions) {
    this.signal = options.signal;
    this.deadline = normalizeDeadline(options.deadline);
  }

  throwIfCancelled(): void {
    if (this.signal?.aborted) {
      throw abortReason(this.signal);
    }
    if (this.deadline !== undefined && Date.now() >= this.deadline) {
      throw deadlineError();
    }
  }

  async wait<T>(operation: Promise<T>): Promise<T> {
    this.throwIfCancelled();

    return await new Promise<T>((resolve, reject) => {
      let settled = false;
      let timer: ReturnType<typeof setTimeout> | undefined;

      const cleanup = () => {
        if (timer !== undefined) clearTimeout(timer);
        this.signal?.removeEventListener('abort', onAbort);
      };
      const finish = (callback: () => void) => {
        if (settled) return;
        settled = true;
        cleanup();
        callback();
      };
      const onAbort = () => finish(() => reject(abortReason(this.signal!)));

      this.signal?.addEventListener('abort', onAbort, { once: true });
      if (this.signal?.aborted) {
        onAbort();
        return;
      }
      if (this.deadline !== undefined) {
        timer = setTimeout(
          () => finish(() => reject(deadlineError())),
          Math.max(0, this.deadline - Date.now()),
        );
      }

      operation.then(
        (value) => finish(() => resolve(value)),
        (error: unknown) => finish(() => reject(error)),
      );
    });
  }
}

export function isCancellationError(error: unknown): boolean {
  return error instanceof DOMException
    && (error.name === 'AbortError' || error.name === 'TimeoutError');
}

function normalizeDeadline(deadline: number | undefined): number | undefined {
  if (deadline === undefined) return undefined;
  if (!Number.isFinite(deadline) || deadline < 0) {
    throw new TypeError('deadline must be an absolute finite epoch timestamp');
  }
  return deadline;
}

function abortReason(signal: AbortSignal): unknown {
  return signal.reason ?? new DOMException('The query was aborted', 'AbortError');
}

function deadlineError(): DOMException {
  return new DOMException('The query deadline elapsed', 'TimeoutError');
}
