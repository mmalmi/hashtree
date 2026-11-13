/** The longest delay `setTimeout` takes without overflowing. */
const MAX_TIMER_MS = 2 ** 31 - 1;

/**
 * Wait for `work` until `deadlineMs` or until `signal` aborts. `work` is not
 * cancelled by this: `abandon` tells its owner that nobody is waiting, and a
 * later failure of an abandoned `work` is ignored.
 */
export function withDeadline<T>(
  work: Promise<T>,
  deadlineMs: number,
  signal: AbortSignal | undefined,
  timeoutMessage: string,
  abandon?: () => void,
): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    let waiting = true;
    const finish = (settle: () => void, abandoned: boolean): void => {
      if (!waiting) return;
      waiting = false;
      clearTimeout(timer);
      signal?.removeEventListener('abort', cancelled);
      if (abandoned) abandon?.();
      settle();
    };
    const cancelled = (): void => {
      finish(() => reject(new Error('paid route read was cancelled')), true);
    };
    const timer = setTimeout(
      () => finish(() => reject(new Error(timeoutMessage)), true),
      Math.min(MAX_TIMER_MS, Math.max(0, deadlineMs - Date.now())),
    );
    if (signal?.aborted) cancelled();
    else signal?.addEventListener('abort', cancelled, { once: true });
    work.then(
      (value) => finish(() => resolve(value), false),
      (error: unknown) => finish(() => reject(error), false),
    );
  });
}
