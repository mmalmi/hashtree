/**
 * Shares one in-flight read between concurrent callers asking for the same key.
 *
 * Only the read itself is shared: an entry is removed the moment it settles,
 * so a result is never served from here later. Joiners see exactly the outcome
 * the first caller sees (data, a miss, or the error), never a substitute miss.
 *
 * Every caller receives its own value. The first gets the loaded value and
 * each joiner a copy, all made before any caller resumes, so one caller
 * transferring or mutating its bytes cannot affect another.
 */
export class InFlightReads<T> {
  private readonly pending = new Map<string, Array<{
    resolve: (value: T) => void;
    reject: (reason: unknown) => void;
  }>>();

  constructor(private readonly copy: (value: T) => T) {}

  get size(): number {
    return this.pending.size;
  }

  read(key: string, load: () => Promise<T>): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      const waiters = this.pending.get(key);
      if (waiters) {
        waiters.push({ resolve, reject });
        return;
      }

      const started = [{ resolve, reject }];
      this.pending.set(key, started);
      const settle = (deliver: (waiter: typeof started[number], index: number) => void): void => {
        if (this.pending.get(key) === started) this.pending.delete(key);
        started.forEach(deliver);
      };
      let loading: Promise<T>;
      try {
        loading = load();
      } catch (error) {
        loading = Promise.reject(error);
      }
      loading
        .then(
          (value) => settle((waiter, index) => {
            try {
              waiter.resolve(index === 0 ? value : this.copy(value));
            } catch (error) {
              waiter.reject(error);
            }
          }),
          (error: unknown) => settle((waiter) => waiter.reject(error)),
        );
    });
  }
}

export function copyBlobBytes(data: Uint8Array | null): Uint8Array | null {
  return data ? data.slice() : data;
}
