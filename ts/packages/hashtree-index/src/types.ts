import type { Hash, Store, CID } from 'hashtree';

export type { Hash, Store, CID };

export interface BTreeOptions {
  /** Max entries per node before splitting. Default: 32 */
  order?: number;
}
