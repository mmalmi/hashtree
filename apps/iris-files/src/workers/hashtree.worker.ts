/**
 * Hashtree Worker Entry Point
 *
 * This file is the entry point for Vite to bundle the hashtree worker.
 */

console.log('[hashtree.worker] Loading worker module...');

// Import worker module from same package
import '../worker/worker';

console.log('[hashtree.worker] Worker module loaded');
