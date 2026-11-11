/**
 * Bounded concurrent chunk loading for ordered file reads.
 */

import { LinkType, type Link, type Store } from '../types.js';

/** Maximum concurrent store reads for one file read. */
export const READ_CONCURRENCY = 16;
/** Maximum declared chunk bytes fetched ahead of the reader. */
export const READ_AHEAD_BYTES = 8 * 1024 * 1024;

function ignoreFailure(): void {}

/**
 * Caps concurrent store reads for one logical read, including nested nodes.
 */
export class ReadLimiter {
  private active = 0;
  private readonly waiting: Array<() => void> = [];

  constructor(private readonly limit: number = READ_CONCURRENCY) {}

  run<T>(task: () => Promise<T>): Promise<T> {
    if (this.active < this.limit) {
      this.active++;
      return this.start(task);
    }
    // The releasing task hands its slot over, so `active` stays unchanged.
    const queued = new Promise<void>((resolve) => this.waiting.push(resolve)).then(() => this.start(task));
    queued.then(undefined, ignoreFailure);
    return queued;
  }

  private start<T>(task: () => Promise<T>): Promise<T> {
    let result: Promise<T>;
    try {
      result = task();
    } catch (error) {
      result = Promise.reject(error);
    }
    // The reader awaits results in link order, so a failure behind the link
    // that ended the read is never awaited. Releasing on both outcomes also
    // marks such a failure as handled instead of an unhandled rejection.
    result.then(this.release, this.release);
    return result;
  }

  private readonly release = (): void => {
    const next = this.waiting.shift();
    if (next) next();
    else this.active--;
  };
}

/**
 * Starts store reads for the links a reader is expected to need, ahead of the
 * reader, while the reader still consumes them strictly in link order.
 *
 * The plan is only a prediction from declared link sizes: a link outside the
 * plan is read on demand, and a planned link the reader skips is dropped.
 */
export class LinkReadAhead {
  private readonly pending = new Map<number, Promise<Uint8Array | null>>();
  private pendingBytes = 0;
  /** Links handed to the reader and not yet followed by the next request. */
  private held = 0;
  private heldBytes = 0;
  private cursor = 0;
  private closed = false;

  constructor(
    private readonly store: Store,
    private readonly links: readonly Link[],
    private readonly planned: readonly number[],
    private readonly limiter: ReadLimiter,
  ) {
    this.fill();
  }

  get(index: number): Promise<Uint8Array | null> {
    for (const earlier of this.pending.keys()) {
      if (earlier >= index) break;
      this.release(earlier);
    }
    while (this.cursor < this.planned.length && this.planned[this.cursor] <= index) {
      this.cursor++;
    }

    const started = this.pending.get(index);
    if (started) this.release(index);
    const promise = started ?? this.start(index);
    this.held = 1;
    this.heldBytes = this.weight(index);
    this.fill();
    return promise;
  }

  close(): void {
    this.closed = true;
    this.pending.clear();
    this.pendingBytes = 0;
  }

  private fill(): void {
    while (!this.closed && this.cursor < this.planned.length) {
      const outstanding = this.pending.size + this.held;
      if (outstanding >= READ_CONCURRENCY) break;
      const index = this.planned[this.cursor];
      const weight = this.weight(index);
      if (outstanding > 0 && this.pendingBytes + this.heldBytes + weight > READ_AHEAD_BYTES) break;
      this.cursor++;
      this.pending.set(index, this.start(index));
      this.pendingBytes += weight;
    }
  }

  private start(index: number): Promise<Uint8Array | null> {
    const { hash } = this.links[index];
    return this.limiter.run(() => (this.closed ? Promise.resolve(null) : this.store.get(hash)));
  }

  private release(index: number): void {
    if (this.pending.delete(index)) this.pendingBytes -= this.weight(index);
  }

  private weight(index: number): number {
    const link = this.links[index];
    return link.type === LinkType.Blob ? Math.max(0, link.size ?? 0) : 0;
  }
}

/**
 * Indexes of the links a `[start, end)` read is expected to load, judged by
 * declared sizes. Planning stops at a link of unknown size, because offsets
 * after it are only known once it has been read.
 */
export function planRangeLinks(links: readonly Link[], start: number, end?: number): number[] {
  const planned: number[] = [];
  if (end !== undefined && end <= start) return planned;

  let position = 0;
  for (let index = 0; index < links.length; index++) {
    if (end !== undefined && position >= end) break;
    const size = links[index].size ?? 0;
    if (size > 0 && position + size <= start) {
      position += size;
      continue;
    }
    planned.push(index);
    if (size <= 0) break;
    position += size;
  }
  return planned;
}

/**
 * Indexes of the links a whole-file read is expected to load before its
 * remaining byte budget, if any, is exhausted.
 */
export function planAllLinks(links: readonly Link[], budget?: number): number[] {
  const planned: number[] = [];
  let declared = 0;
  for (let index = 0; index < links.length; index++) {
    declared += Math.max(0, links[index].size ?? 0);
    if (budget !== undefined && declared > budget) break;
    planned.push(index);
  }
  return planned;
}
