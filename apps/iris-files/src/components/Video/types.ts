/**
 * Video metadata stored in metadata.json within video tree
 */
export interface VideoMetadata {
  title: string;
  description?: string;
  duration?: number; // seconds
  uploadedAt?: number; // timestamp ms
  mimeType?: string;
  width?: number;
  height?: number;
  tags?: string[];
}

/**
 * Video item for display in grids/lists
 */
export interface VideoItem {
  key: string;
  title: string;
  description?: string;
  duration?: number;
  ownerPubkey: string | null;
  ownerNpub: string | null;
  treeName: string;
  /** For playlist videos: the video folder name within the playlist tree */
  videoId?: string;
  /** Root CID for playlist detection */
  rootCid?: import('hashtree').CID;
  visibility?: string;
  href: string;
  timestamp?: number;
  /** Pubkey of user who reacted/commented (for social feed items) */
  reactorPubkey?: string;
}
