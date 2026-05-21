// @ts-nocheck
/**
 * Extract error message from unknown error type
 */
export function getErrorMessage(err) {
    return err instanceof Error ? err.message : String(err);
}
//# sourceMappingURL=errorMessage.js.map