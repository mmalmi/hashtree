/**
 * SortedMap - maintains items in sorted order with O(log n) insert position lookup
 *
 * Based on iris-client's SortedMap implementation.
 * Uses binary search to find insertion position, maintains sorted array.
 */

type Comparator<K, V> = (a: [K, V], b: [K, V]) => number;

export class SortedMap<K, V> {
  private map: Map<K, V>;
  private sortedKeys: K[];
  private keyToIndex: Map<K, number>;
  private compare: Comparator<K, V>;

  constructor(
    compare: Comparator<K, V>,
    initialEntries?: Iterable<readonly [K, V]>
  ) {
    this.map = new Map(initialEntries || []);
    this.keyToIndex = new Map();
    this.compare = compare;

    this.sortedKeys = initialEntries
      ? [...this.map.entries()].sort(this.compare).map(([key]) => key)
      : [];

    // Build initial index map
    this.sortedKeys.forEach((key, idx) => this.keyToIndex.set(key, idx));
  }

  private binarySearch(key: K, value: V): number {
    let left = 0;
    let right = this.sortedKeys.length;
    while (left < right) {
      const mid = (left + right) >> 1;
      const midKey = this.sortedKeys[mid];
      const midValue = this.map.get(midKey) as V;

      if (this.compare([key, value], [midKey, midValue]) < 0) {
        right = mid;
      } else {
        left = mid + 1;
      }
    }
    return left;
  }

  private updateIndexRange(start: number, end: number) {
    for (let i = start; i < end; i++) {
      this.keyToIndex.set(this.sortedKeys[i], i);
    }
  }

  set(key: K, value: V): void {
    const existingIndex = this.keyToIndex.get(key);
    this.map.set(key, value);

    if (existingIndex !== undefined) {
      // Remove from old position
      this.sortedKeys.splice(existingIndex, 1);
      this.updateIndexRange(existingIndex, this.sortedKeys.length);
    }

    const insertAt = this.binarySearch(key, value);
    this.sortedKeys.splice(insertAt, 0, key);
    this.updateIndexRange(insertAt, this.sortedKeys.length);
  }

  get(key: K): V | undefined {
    return this.map.get(key);
  }

  has(key: K): boolean {
    return this.map.has(key);
  }

  delete(key: K): boolean {
    if (this.map.delete(key)) {
      const index = this.keyToIndex.get(key);
      if (index !== undefined) {
        this.sortedKeys.splice(index, 1);
        this.keyToIndex.delete(key);
        this.updateIndexRange(index, this.sortedKeys.length);
      }
      return true;
    }
    return false;
  }

  clear(): void {
    this.map.clear();
    this.sortedKeys = [];
    this.keyToIndex.clear();
  }

  get size(): number {
    return this.map.size;
  }

  /** Get all values in sorted order */
  values(): V[] {
    return this.sortedKeys.map(key => this.map.get(key) as V);
  }

  /** Get all entries in sorted order */
  entries(): [K, V][] {
    return this.sortedKeys.map(key => [key, this.map.get(key) as V]);
  }

  *[Symbol.iterator](): Iterator<[K, V]> {
    for (const key of this.sortedKeys) {
      yield [key, this.map.get(key) as V];
    }
  }
}
