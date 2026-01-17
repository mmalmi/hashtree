/**
 * A simple keyed event emitter for pub/sub patterns.
 * Listeners can subscribe to specific keys and get notified when values are emitted for those keys.
 */
export class KeyedEventEmitter<K, V> {
  private listeners = new Map<K, Set<(value: V) => void>>();

  /**
   * Subscribe to events for a specific key.
   * @returns Unsubscribe function
   */
  subscribe(key: K, listener: (value: V) => void): () => void {
    let set = this.listeners.get(key);
    if (!set) {
      set = new Set();
      this.listeners.set(key, set);
    }
    set.add(listener);
    return () => {
      set!.delete(listener);
      if (set!.size === 0) {
        this.listeners.delete(key);
      }
    };
  }

  /**
   * Notify all listeners for a specific key with a value.
   */
  notify(key: K, value: V): void {
    const set = this.listeners.get(key);
    if (set) {
      set.forEach(fn => fn(value));
    }
  }

  /**
   * Check if there are any listeners for a key.
   */
  hasListeners(key: K): boolean {
    const set = this.listeners.get(key);
    return set !== undefined && set.size > 0;
  }

  /**
   * Get the number of listeners for a key.
   */
  listenerCount(key: K): number {
    return this.listeners.get(key)?.size ?? 0;
  }

  /**
   * Remove all listeners for all keys.
   */
  clear(): void {
    this.listeners.clear();
  }
}
