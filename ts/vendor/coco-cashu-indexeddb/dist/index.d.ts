import Dexie, { Transaction } from "dexie";
import { CoreProof, Counter, CounterRepository, HistoryEntry, KeyRingRepository, Keypair, Keyset, KeysetRepository, MeltHistoryEntry, MeltQuote, MeltQuoteRepository, Mint, MintHistoryEntry, MintQuote, MintQuoteRepository, MintRepository, ProofRepository, ProofState, ReceiveHistoryEntry, Repositories, RepositoryTransactionScope, SendHistoryEntry } from "coco-cashu-core";

//#region src/lib/db.d.ts
interface IdbDbOptions {
  name?: string;
}
/**
 * Wrapper around Dexie providing transaction management for IndexedDB.
 *
 * Transaction behavior:
 * - Nested transactions within the same Dexie transaction context are reused
 * - Concurrent transactions are queued and executed serially
 * - Dexie handles automatic commit/rollback based on promise resolution/rejection
 */
declare class IdbDb extends Dexie {
  /** Promise chain used to serialize concurrent transactions */
  private transactionQueue;
  /** Currently active Dexie transaction (null if no transaction) */
  private activeTransaction;
  constructor(options?: IdbDbOptions);
  /**
   * Execute a function within a database transaction.
   *
   * Transaction Semantics:
   *
   * 1. NESTED TRANSACTIONS (same Dexie context):
   *    When runTransaction() is called from within an active transaction,
   *    Dexie.currentTransaction will be set. The inner call reuses this transaction.
   *    No new transaction is created.
   *
   * 2. CONCURRENT TRANSACTIONS (different contexts):
   *    When runTransaction() is called while another transaction is active but from
   *    a different context, the new transaction waits in a queue. This prevents
   *    conflicts and ensures serialization of operations.
   *
   * 3. ERROR HANDLING:
   *    Dexie automatically rolls back the transaction if the promise is rejected.
   *    The transaction queue is properly released even on error, allowing subsequent
   *    transactions to proceed.
   *
   * @param mode - Transaction mode: 'r' (readonly) or 'rw' (readwrite)
   * @param stores - Array of store names to include in the transaction
   * @param fn - Function to execute within the transaction, receives a Dexie transaction
   * @returns Promise that resolves with the return value of fn
   * @throws Re-throws any error from fn after Dexie rolls back the transaction
   */
  runTransaction<T>(mode: 'r' | 'rw', stores: string[], fn: (txDb: Transaction) => Promise<T>): Promise<T>;
  get currentTransaction(): Transaction | null;
}
//#endregion
//#region src/lib/schema.d.ts
declare function ensureSchema(db: IdbDb): Promise<void>;
//#endregion
//#region src/repositories/MintRepository.d.ts
declare class IdbMintRepository implements MintRepository {
  private readonly db;
  constructor(db: IdbDb);
  isTrustedMint(mintUrl: string): Promise<boolean>;
  getMintByUrl(mintUrl: string): Promise<Mint>;
  getAllMints(): Promise<Mint[]>;
  getAllTrustedMints(): Promise<Mint[]>;
  addNewMint(mint: Mint): Promise<void>;
  addOrUpdateMint(mint: Mint): Promise<void>;
  updateMint(mint: Mint): Promise<void>;
  setMintTrusted(mintUrl: string, trusted: boolean): Promise<void>;
  deleteMint(mintUrl: string): Promise<void>;
}
//#endregion
//#region src/repositories/KeysetRepository.d.ts
declare class IdbKeysetRepository implements KeysetRepository {
  private readonly db;
  constructor(db: IdbDb);
  getKeysetsByMintUrl(mintUrl: string): Promise<Keyset[]>;
  getKeysetById(mintUrl: string, id: string): Promise<Keyset | null>;
  updateKeyset(keyset: Omit<Keyset, 'keypairs' | 'updatedAt'>): Promise<void>;
  addKeyset(keyset: Omit<Keyset, 'updatedAt'>): Promise<void>;
  deleteKeyset(mintUrl: string, keysetId: string): Promise<void>;
}
//#endregion
//#region src/repositories/KeyRingRepository.d.ts
declare class IdbKeyRingRepository implements KeyRingRepository {
  private readonly db;
  constructor(db: IdbDb);
  getPersistedKeyPair(publicKey: string): Promise<Keypair | null>;
  setPersistedKeyPair(keyPair: Keypair): Promise<void>;
  deletePersistedKeyPair(publicKey: string): Promise<void>;
  getAllPersistedKeyPairs(): Promise<Keypair[]>;
  getLatestKeyPair(): Promise<Keypair | null>;
  getLastDerivationIndex(): Promise<number>;
}
//#endregion
//#region src/repositories/CounterRepository.d.ts
declare class IdbCounterRepository implements CounterRepository {
  private readonly db;
  constructor(db: IdbDb);
  getCounter(mintUrl: string, keysetId: string): Promise<Counter | null>;
  setCounter(mintUrl: string, keysetId: string, counter: number): Promise<void>;
}
//#endregion
//#region src/repositories/ProofRepository.d.ts
declare class IdbProofRepository implements ProofRepository {
  private readonly db;
  constructor(db: IdbDb);
  saveProofs(mintUrl: string, proofs: CoreProof[]): Promise<void>;
  getReadyProofs(mintUrl: string): Promise<CoreProof[]>;
  getAllReadyProofs(): Promise<CoreProof[]>;
  getProofsByKeysetId(mintUrl: string, keysetId: string): Promise<CoreProof[]>;
  setProofState(mintUrl: string, secrets: string[], state: ProofState): Promise<void>;
  deleteProofs(mintUrl: string, secrets: string[]): Promise<void>;
  wipeProofsByKeysetId(mintUrl: string, keysetId: string): Promise<void>;
}
//#endregion
//#region src/repositories/MintQuoteRepository.d.ts
declare class IdbMintQuoteRepository implements MintQuoteRepository {
  private readonly db;
  constructor(db: IdbDb);
  getMintQuote(mintUrl: string, quoteId: string): Promise<MintQuote | null>;
  addMintQuote(quote: MintQuote): Promise<void>;
  setMintQuoteState(mintUrl: string, quoteId: string, state: MintQuote['state']): Promise<void>;
  getPendingMintQuotes(): Promise<MintQuote[]>;
}
//#endregion
//#region src/repositories/MeltQuoteRepository.d.ts
declare class IdbMeltQuoteRepository implements MeltQuoteRepository {
  private readonly db;
  constructor(db: IdbDb);
  getMeltQuote(mintUrl: string, quoteId: string): Promise<MeltQuote | null>;
  addMeltQuote(quote: MeltQuote): Promise<void>;
  setMeltQuoteState(mintUrl: string, quoteId: string, state: MeltQuote['state']): Promise<void>;
  getPendingMeltQuotes(): Promise<MeltQuote[]>;
}
//#endregion
//#region src/repositories/HistoryRepository.d.ts
type NewHistoryEntry = Omit<MintHistoryEntry, 'id'> | Omit<MeltHistoryEntry, 'id'> | Omit<SendHistoryEntry, 'id'> | Omit<ReceiveHistoryEntry, 'id'>;
type UpdatableHistoryEntry = Omit<MintHistoryEntry, 'id' | 'createdAt'> | Omit<MeltHistoryEntry, 'id' | 'createdAt'>;
declare class IdbHistoryRepository {
  private readonly db;
  constructor(db: IdbDb);
  getPaginatedHistoryEntries(limit: number, offset: number): Promise<HistoryEntry[]>;
  addHistoryEntry(history: NewHistoryEntry): Promise<HistoryEntry>;
  getMintHistoryEntry(mintUrl: string, quoteId: string): Promise<MintHistoryEntry | null>;
  getMeltHistoryEntry(mintUrl: string, quoteId: string): Promise<MeltHistoryEntry | null>;
  updateHistoryEntry(history: UpdatableHistoryEntry): Promise<HistoryEntry>;
  deleteHistoryEntry(mintUrl: string, quoteId: string): Promise<void>;
  private entryToRow;
  private rowToEntry;
}
//#endregion
//#region src/index.d.ts
interface IndexedDbRepositoriesOptions extends IdbDbOptions {}
declare class IndexedDbRepositories implements Repositories {
  readonly mintRepository: MintRepository;
  readonly keyRingRepository: KeyRingRepository;
  readonly counterRepository: CounterRepository;
  readonly keysetRepository: KeysetRepository;
  readonly proofRepository: ProofRepository;
  readonly mintQuoteRepository: MintQuoteRepository;
  readonly meltQuoteRepository: MeltQuoteRepository;
  readonly historyRepository: IdbHistoryRepository;
  readonly db: IdbDb;
  constructor(options: IndexedDbRepositoriesOptions);
  init(): Promise<void>;
  withTransaction<T>(fn: (repos: RepositoryTransactionScope) => Promise<T>): Promise<T>;
}
//#endregion
export { IdbCounterRepository, IdbDb, IdbHistoryRepository, IdbKeyRingRepository, IdbKeysetRepository, IdbMeltQuoteRepository, IdbMintQuoteRepository, IdbMintRepository, IdbProofRepository, IndexedDbRepositories, IndexedDbRepositoriesOptions, ensureSchema };