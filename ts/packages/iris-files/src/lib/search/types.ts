/**
 * Search types and interfaces
 */

/** Unified search result from any provider */
export interface SearchResult {
  /** Unique ID (provider:id format) */
  id: string;
  /** Result type for rendering */
  type: 'history' | 'user' | 'video' | 'file' | 'tree';
  /** Primary display text */
  label: string;
  /** Secondary text (e.g., "Visited 2h ago") */
  sublabel?: string;
  /** Navigation path */
  path: string;
  /** Relevance score 0-1 */
  score: number;
  /** Icon class */
  icon?: string;
  /** For user results */
  pubkey?: string;
  /** For video results */
  thumbnail?: string;
  /** Timestamp for recency display */
  timestamp?: number;
}

/** Search provider interface */
export interface SearchProvider {
  /** Unique provider ID */
  id: string;
  /** Human-readable name */
  name: string;
  /** Search for results */
  search(query: string, limit: number): Promise<SearchResult[]>;
  /** Optional priority (higher = shown first when scores equal) */
  priority?: number;
  /** Whether this provider is available */
  isAvailable(): boolean;
}

/** Search options */
export interface SearchOptions {
  /** Limit total results */
  limit?: number;
  /** Only search specific providers */
  sources?: string[];
  /** Include results with score below threshold */
  minScore?: number;
}
