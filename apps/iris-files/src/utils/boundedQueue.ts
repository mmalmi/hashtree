/**
 * BoundedQueue - Memory-safe queue with size limits
 *
 * Prevents memory blowup by enforcing both item count and byte limits.
 * When limits are exceeded, oldest items are dropped (FIFO eviction).
 *
 * Use this instead of plain arrays for queues that could grow unbounded,
 * especially for network buffers, send queues, and work queues.
 */

export interface BoundedQueueOptions<T> {
  /** Maximum number of items in queue */
  maxItems: number;
  /** Maximum total bytes in queue */
  maxBytes: number;
  /** Function to get byte size of an item */
  getBytes: (item: T) => number;
  /** Optional callback when items are dropped due to overflow */
  onDrop?: (item: T, reason: 'items' | 'bytes') => void;
}

export class BoundedQueue<T> {
  private items: T[] = [];
  private bytesUsed = 0;
  private readonly maxItems: number;
  private readonly maxBytes: number;
  private readonly getBytes: (item: T) => number;
  private readonly onDrop?: (item: T, reason: 'items' | 'bytes') => void;

  constructor(options: BoundedQueueOptions<T>) {
    this.maxItems = options.maxItems;
    this.maxBytes = options.maxBytes;
    this.getBytes = options.getBytes;
    this.onDrop = options.onDrop;
  }

  /**
   * Add item to queue, dropping oldest items if limits exceeded
   * @returns Number of items dropped to make room
   */
  push(item: T): number {
    const itemBytes = this.getBytes(item);
    let dropped = 0;

    // Drop oldest items until we have room
    while (
      this.items.length > 0 &&
      (this.items.length >= this.maxItems || this.bytesUsed + itemBytes > this.maxBytes)
    ) {
      const droppedItem = this.items.shift()!;
      const droppedBytes = this.getBytes(droppedItem);
      this.bytesUsed -= droppedBytes;
      dropped++;

      if (this.onDrop) {
        const reason = this.items.length >= this.maxItems ? 'items' : 'bytes';
        this.onDrop(droppedItem, reason);
      }
    }

    this.items.push(item);
    this.bytesUsed += itemBytes;
    return dropped;
  }

  /**
   * Remove and return oldest item, or undefined if empty
   */
  shift(): T | undefined {
    const item = this.items.shift();
    if (item !== undefined) {
      this.bytesUsed -= this.getBytes(item);
    }
    return item;
  }

  /**
   * Peek at oldest item without removing
   */
  peek(): T | undefined {
    return this.items[0];
  }

  /**
   * Clear all items
   */
  clear(): void {
    this.items = [];
    this.bytesUsed = 0;
  }

  /**
   * Get current item count
   */
  get length(): number {
    return this.items.length;
  }

  /**
   * Get current byte usage
   */
  get bytes(): number {
    return this.bytesUsed;
  }

  /**
   * Check if queue is empty
   */
  get isEmpty(): boolean {
    return this.items.length === 0;
  }

  /**
   * Check if queue is at item capacity
   */
  get isFullItems(): boolean {
    return this.items.length >= this.maxItems;
  }

  /**
   * Check if queue is at byte capacity
   */
  get isFullBytes(): boolean {
    return this.bytesUsed >= this.maxBytes;
  }

  /**
   * Iterate over items (does not remove them)
   */
  *[Symbol.iterator](): Iterator<T> {
    yield* this.items;
  }

  /**
   * Get all items as array (for iteration/reduce operations)
   */
  toArray(): T[] {
    return [...this.items];
  }
}
