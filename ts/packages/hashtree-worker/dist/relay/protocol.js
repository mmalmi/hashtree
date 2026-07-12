// @ts-nocheck
/**
 * Worker Protocol Types
 *
 * Message types for communication between main thread and hashtree worker.
 * Worker owns: HashTree and Nostr (via nostr-tools)
 * Main thread owns: UI, NIP-07 extension access (signing/encryption)
 */
// ============================================================================
// Helper functions
// ============================================================================
let requestIdCounter = 0;
export function generateRequestId() {
    return `req_${Date.now()}_${++requestIdCounter}`;
}
//# sourceMappingURL=protocol.js.map