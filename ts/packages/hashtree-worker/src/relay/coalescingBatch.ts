/**
 * Coalesces a burst of keyed candidates into rounds.
 *
 * Only the winning candidate per key is kept while a round is being applied,
 * so work queued behind a slow round collapses to one item per key. Rounds
 * are applied one at a time, in the order they were formed.
 */
export class CoalescingBatch<T> {
  private pending = new Map<string, T>();
  private draining: Promise<void> | null = null;

  constructor(
    private readonly supersedes: (candidate: T, current: T) => boolean,
    private readonly apply: (round: T[]) => Promise<void>,
  ) {}

  /** Resolves once the candidate has been applied or superseded. */
  offer(key: string, candidate: T): Promise<void> {
    const current = this.pending.get(key);
    if (current === undefined || this.supersedes(candidate, current)) {
      this.pending.set(key, candidate);
    }
    return this.settled();
  }

  /** Resolves once everything offered before this call has been applied. */
  settled(): Promise<void> {
    if (!this.draining) {
      if (this.pending.size === 0) return Promise.resolve();
      this.draining = this.drain().finally(() => {
        this.draining = null;
      });
    }
    return this.draining;
  }

  private async drain(): Promise<void> {
    // Let offers made in the same tick join the first round.
    await Promise.resolve();
    let failure: unknown;
    while (this.pending.size > 0) {
      const round = [...this.pending.values()];
      this.pending = new Map();
      try {
        await this.apply(round);
      } catch (error) {
        failure ??= error;
      }
    }
    if (failure) throw failure;
  }
}
