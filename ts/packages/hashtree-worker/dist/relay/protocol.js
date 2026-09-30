// ============================================================================
// Helper functions
// ============================================================================
let requestIdCounter = 0;
export function generateRequestId() {
    return `req_${Date.now()}_${++requestIdCounter}`;
}
//# sourceMappingURL=protocol.js.map