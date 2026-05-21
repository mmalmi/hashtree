/**
 * BoundedQueue - Memory-safe queue with size limits
 *
 * Prevents memory blowup by enforcing both item count and byte limits.
 * When limits are exceeded, oldest items are dropped (FIFO eviction).
 *
 * Use this instead of plain arrays for queues that could grow unbounded,
 * especially for network buffers, send queues, and work queues.
 */
export class BoundedQueue {
    items = [];
    bytesUsed = 0;
    maxItems;
    maxBytes;
    getBytes;
    onDrop;
    constructor(options) {
        this.maxItems = options.maxItems;
        this.maxBytes = options.maxBytes;
        this.getBytes = options.getBytes;
        this.onDrop = options.onDrop;
    }
    /**
     * Add item to queue, dropping oldest items if limits exceeded
     * @returns Number of items dropped to make room
     */
    push(item) {
        const itemBytes = this.getBytes(item);
        let dropped = 0;
        // Drop oldest items until we have room
        while (this.items.length > 0 &&
            (this.items.length >= this.maxItems || this.bytesUsed + itemBytes > this.maxBytes)) {
            const droppedItem = this.items.shift();
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
     * Add item to the front of the queue, dropping items from the back if limits are exceeded.
     * Useful for urgent control messages that should overtake bulk background traffic.
     */
    unshift(item) {
        const itemBytes = this.getBytes(item);
        let dropped = 0;
        while (this.items.length > 0 &&
            (this.items.length >= this.maxItems || this.bytesUsed + itemBytes > this.maxBytes)) {
            const droppedItem = this.items.pop();
            const droppedBytes = this.getBytes(droppedItem);
            this.bytesUsed -= droppedBytes;
            dropped++;
            if (this.onDrop) {
                const reason = this.items.length >= this.maxItems ? 'items' : 'bytes';
                this.onDrop(droppedItem, reason);
            }
        }
        this.items.unshift(item);
        this.bytesUsed += itemBytes;
        return dropped;
    }
    /**
     * Remove and return oldest item, or undefined if empty
     */
    shift() {
        const item = this.items.shift();
        if (item !== undefined) {
            this.bytesUsed -= this.getBytes(item);
        }
        return item;
    }
    /**
     * Peek at oldest item without removing
     */
    peek() {
        return this.items[0];
    }
    /**
     * Clear all items
     */
    clear() {
        this.items = [];
        this.bytesUsed = 0;
    }
    /**
     * Get current item count
     */
    get length() {
        return this.items.length;
    }
    /**
     * Get current byte usage
     */
    get bytes() {
        return this.bytesUsed;
    }
    /**
     * Check if queue is empty
     */
    get isEmpty() {
        return this.items.length === 0;
    }
    /**
     * Check if queue is at item capacity
     */
    get isFullItems() {
        return this.items.length >= this.maxItems;
    }
    /**
     * Check if queue is at byte capacity
     */
    get isFullBytes() {
        return this.bytesUsed >= this.maxBytes;
    }
    /**
     * Iterate over items (does not remove them)
     */
    *[Symbol.iterator]() {
        yield* this.items;
    }
    /**
     * Get all items as array (for iteration/reduce operations)
     */
    toArray() {
        return [...this.items];
    }
}
//# sourceMappingURL=boundedQueue.js.map