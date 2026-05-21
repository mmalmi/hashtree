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
export declare class BoundedQueue<T> {
    private items;
    private bytesUsed;
    private readonly maxItems;
    private readonly maxBytes;
    private readonly getBytes;
    private readonly onDrop?;
    constructor(options: BoundedQueueOptions<T>);
    /**
     * Add item to queue, dropping oldest items if limits exceeded
     * @returns Number of items dropped to make room
     */
    push(item: T): number;
    /**
     * Add item to the front of the queue, dropping items from the back if limits are exceeded.
     * Useful for urgent control messages that should overtake bulk background traffic.
     */
    unshift(item: T): number;
    /**
     * Remove and return oldest item, or undefined if empty
     */
    shift(): T | undefined;
    /**
     * Peek at oldest item without removing
     */
    peek(): T | undefined;
    /**
     * Clear all items
     */
    clear(): void;
    /**
     * Get current item count
     */
    get length(): number;
    /**
     * Get current byte usage
     */
    get bytes(): number;
    /**
     * Check if queue is empty
     */
    get isEmpty(): boolean;
    /**
     * Check if queue is at item capacity
     */
    get isFullItems(): boolean;
    /**
     * Check if queue is at byte capacity
     */
    get isFullBytes(): boolean;
    /**
     * Iterate over items (does not remove them)
     */
    [Symbol.iterator](): Iterator<T>;
    /**
     * Get all items as array (for iteration/reduce operations)
     */
    toArray(): T[];
}
//# sourceMappingURL=boundedQueue.d.ts.map