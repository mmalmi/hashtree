import { CashuMint, CashuWallet, MeltQuoteResponse, MeltQuoteState, MintKeys, MintKeyset, MintQuoteResponse, MintQuoteState, OutputData, Proof, Token, getDecodedToken, getEncodedToken } from "@cashu/cashu-ts";

//#region types.d.ts
type MintInfo = Awaited<ReturnType<CashuMint['getInfo']>>;
type ProofState = 'inflight' | 'ready' | 'spent';
interface CoreProof extends Proof {
  mintUrl: string;
  state: ProofState;
}
//#endregion
//#region models/Mint.d.ts
interface Mint {
  mintUrl: string;
  name: string;
  mintInfo: MintInfo;
  trusted: boolean;
  createdAt: number;
  updatedAt: number;
}
//#endregion
//#region models/Keyset.d.ts
interface Keyset {
  mintUrl: string;
  id: string;
  unit: string;
  keypairs: Record<number, string>;
  active: boolean;
  feePpk: number;
  updatedAt: number;
}
//#endregion
//#region models/Counter.d.ts
interface Counter {
  mintUrl: string;
  keysetId: string;
  counter: number;
}
//#endregion
//#region models/MintQuote.d.ts
interface MintQuote extends MintQuoteResponse {
  mintUrl: string;
}
//#endregion
//#region models/MeltQuote.d.ts
interface MeltQuote extends MeltQuoteResponse {
  mintUrl: string;
}
//#endregion
//#region models/History.d.ts
type BaseHistoryEntry = {
  id: string;
  createdAt: number;
  mintUrl: string;
  unit: string;
  metadata?: Record<string, string>;
};
type MintHistoryEntry = BaseHistoryEntry & {
  type: 'mint';
  paymentRequest: string;
  quoteId: string;
  state: MintQuoteState;
  amount: number;
};
type MeltHistoryEntry = BaseHistoryEntry & {
  type: 'melt';
  quoteId: string;
  state: MeltQuoteState;
  amount: number;
};
type SendHistoryEntry = BaseHistoryEntry & {
  type: 'send';
  amount: number;
  token: Token;
};
type ReceiveHistoryEntry = BaseHistoryEntry & {
  type: 'receive';
  amount: number;
};
type HistoryEntry = MintHistoryEntry | MeltHistoryEntry | SendHistoryEntry | ReceiveHistoryEntry;
//#endregion
//#region models/Keypair.d.ts
type Keypair = {
  publicKeyHex: string;
  secretKey: Uint8Array;
  derivationIndex?: number;
};
//#endregion
//#region repositories/memory/MemoryCounterRepository.d.ts
declare class MemoryCounterRepository implements CounterRepository {
  private counters;
  private key;
  getCounter(mintUrl: string, keysetId: string): Promise<Counter | null>;
  setCounter(mintUrl: string, keysetId: string, counter: number): Promise<void>;
}
//#endregion
//#region repositories/memory/MemoryKeysetRepository.d.ts
declare class MemoryKeysetRepository implements KeysetRepository {
  private keysetsByMint;
  private getMintMap;
  getKeysetsByMintUrl(mintUrl: string): Promise<Keyset[]>;
  getKeysetById(mintUrl: string, id: string): Promise<Keyset | null>;
  updateKeyset(keyset: Omit<Keyset, 'keypairs' | 'updatedAt'>): Promise<void>;
  addKeyset(keyset: Omit<Keyset, 'updatedAt'>): Promise<void>;
  deleteKeyset(mintUrl: string, keysetId: string): Promise<void>;
}
//#endregion
//#region repositories/memory/MemoryKeyRingRepository.d.ts
declare class MemoryKeyRingRepository implements KeyRingRepository {
  private keyPairs;
  private insertionOrder;
  getPersistedKeyPair(publicKey: string): Promise<Keypair | null>;
  setPersistedKeyPair(keyPair: Keypair): Promise<void>;
  deletePersistedKeyPair(publicKey: string): Promise<void>;
  getAllPersistedKeyPairs(): Promise<Keypair[]>;
  getLatestKeyPair(): Promise<Keypair | null>;
  getLastDerivationIndex(): Promise<number>;
}
//#endregion
//#region repositories/memory/MemoryMintRepository.d.ts
declare class MemoryMintRepository implements MintRepository {
  private mints;
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
//#region repositories/memory/MemoryProofRepository.d.ts
type ProofState$1 = 'inflight' | 'ready' | 'spent';
declare class MemoryProofRepository implements ProofRepository {
  private proofsByMint;
  private getMintMap;
  saveProofs(mintUrl: string, proofs: CoreProof[]): Promise<void>;
  getReadyProofs(mintUrl: string): Promise<CoreProof[]>;
  getAllReadyProofs(): Promise<CoreProof[]>;
  getProofsByKeysetId(mintUrl: string, keysetId: string): Promise<CoreProof[]>;
  setProofState(mintUrl: string, secrets: string[], state: ProofState$1): Promise<void>;
  deleteProofs(mintUrl: string, secrets: string[]): Promise<void>;
  wipeProofsByKeysetId(mintUrl: string, keysetId: string): Promise<void>;
}
//#endregion
//#region repositories/memory/MemoryRepositories.d.ts
declare class MemoryRepositories implements Repositories {
  mintRepository: MintRepository;
  keyRingRepository: KeyRingRepository;
  counterRepository: CounterRepository;
  keysetRepository: KeysetRepository;
  proofRepository: ProofRepository;
  mintQuoteRepository: MintQuoteRepository;
  meltQuoteRepository: MeltQuoteRepository;
  historyRepository: HistoryRepository;
  constructor();
  init(): Promise<void>;
  withTransaction<T>(fn: (repos: RepositoryTransactionScope) => Promise<T>): Promise<T>;
}
//#endregion
//#region repositories/memory/MemoryMintQuoteRepository.d.ts
declare class MemoryMintQuoteRepository implements MintQuoteRepository {
  private readonly quotes;
  private makeKey;
  getMintQuote(mintUrl: string, quoteId: string): Promise<MintQuote | null>;
  addMintQuote(quote: MintQuote): Promise<void>;
  setMintQuoteState(mintUrl: string, quoteId: string, state: MintQuote['state']): Promise<void>;
  getPendingMintQuotes(): Promise<MintQuote[]>;
}
//#endregion
//#region repositories/memory/MemoryMeltQuoteRepository.d.ts
declare class MemoryMeltQuoteRepository implements MeltQuoteRepository {
  private readonly quotes;
  private makeKey;
  getMeltQuote(mintUrl: string, quoteId: string): Promise<MeltQuote | null>;
  addMeltQuote(quote: MeltQuote): Promise<void>;
  setMeltQuoteState(mintUrl: string, quoteId: string, state: MeltQuote['state']): Promise<void>;
  getPendingMeltQuotes(): Promise<MeltQuote[]>;
}
//#endregion
//#region repositories/memory/MemoryHistoryRepository.d.ts
type NewHistoryEntry = Omit<MintHistoryEntry, 'id'> | Omit<MeltHistoryEntry, 'id'> | Omit<SendHistoryEntry, 'id'> | Omit<ReceiveHistoryEntry, 'id'>;
declare class MemoryHistoryRepository implements HistoryRepository {
  private readonly entries;
  private nextId;
  getPaginatedHistoryEntries(limit: number, offset: number): Promise<HistoryEntry[]>;
  addHistoryEntry(history: NewHistoryEntry): Promise<HistoryEntry>;
  getMintHistoryEntry(mintUrl: string, quoteId: string): Promise<MintHistoryEntry | null>;
  getMeltHistoryEntry(mintUrl: string, quoteId: string): Promise<MeltHistoryEntry | null>;
  updateHistoryEntry(history: Omit<MintHistoryEntry, 'id' | 'createdAt'> | Omit<MeltHistoryEntry, 'id' | 'createdAt'>): Promise<HistoryEntry>;
  deleteHistoryEntry(mintUrl: string, quoteId: string): Promise<void>;
}
//#endregion
//#region repositories/index.d.ts
interface MintRepository {
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
interface KeysetRepository {
  getKeysetsByMintUrl(mintUrl: string): Promise<Keyset[]>;
  getKeysetById(mintUrl: string, id: string): Promise<Keyset | null>;
  updateKeyset(keyset: Omit<Keyset, 'keypairs' | 'updatedAt'>): Promise<void>;
  addKeyset(keyset: Omit<Keyset, 'updatedAt'>): Promise<void>;
  deleteKeyset(mintUrl: string, keysetId: string): Promise<void>;
}
interface CounterRepository {
  getCounter(mintUrl: string, keysetId: string): Promise<Counter | null>;
  setCounter(mintUrl: string, keysetId: string, counter: number): Promise<void>;
}
interface ProofRepository {
  saveProofs(mintUrl: string, proofs: CoreProof[]): Promise<void>;
  getReadyProofs(mintUrl: string): Promise<CoreProof[]>;
  getAllReadyProofs(): Promise<CoreProof[]>;
  setProofState(mintUrl: string, secrets: string[], state: ProofState): Promise<void>;
  deleteProofs(mintUrl: string, secrets: string[]): Promise<void>;
  getProofsByKeysetId(mintUrl: string, keysetId: string): Promise<CoreProof[]>;
  wipeProofsByKeysetId(mintUrl: string, keysetId: string): Promise<void>;
}
interface MintQuoteRepository {
  getMintQuote(mintUrl: string, quoteId: string): Promise<MintQuote | null>;
  addMintQuote(quote: MintQuote): Promise<void>;
  setMintQuoteState(mintUrl: string, quoteId: string, state: MintQuote['state']): Promise<void>;
  getPendingMintQuotes(): Promise<MintQuote[]>;
}
interface KeyRingRepository {
  getPersistedKeyPair(publicKey: string): Promise<Keypair | null>;
  setPersistedKeyPair(keyPair: Keypair): Promise<void>;
  deletePersistedKeyPair(publicKey: string): Promise<void>;
  getAllPersistedKeyPairs(): Promise<Keypair[]>;
  getLatestKeyPair(): Promise<Keypair | null>;
  getLastDerivationIndex(): Promise<number>;
}
interface MeltQuoteRepository {
  getMeltQuote(mintUrl: string, quoteId: string): Promise<MeltQuote | null>;
  addMeltQuote(quote: MeltQuote): Promise<void>;
  setMeltQuoteState(mintUrl: string, quoteId: string, state: MeltQuote['state']): Promise<void>;
  getPendingMeltQuotes(): Promise<MeltQuote[]>;
}
interface HistoryRepository {
  getPaginatedHistoryEntries(limit: number, offset: number): Promise<HistoryEntry[]>;
  addHistoryEntry(history: Omit<HistoryEntry, 'id'>): Promise<HistoryEntry>;
  getMintHistoryEntry(mintUrl: string, quoteId: string): Promise<MintHistoryEntry | null>;
  getMeltHistoryEntry(mintUrl: string, quoteId: string): Promise<MeltHistoryEntry | null>;
  updateHistoryEntry(history: Omit<HistoryEntry, 'id' | 'createdAt'>): Promise<HistoryEntry>;
  deleteHistoryEntry(mintUrl: string, quoteId: string): Promise<void>;
}
interface RepositoriesBase {
  mintRepository: MintRepository;
  keyRingRepository: KeyRingRepository;
  counterRepository: CounterRepository;
  keysetRepository: KeysetRepository;
  proofRepository: ProofRepository;
  mintQuoteRepository: MintQuoteRepository;
  meltQuoteRepository: MeltQuoteRepository;
  historyRepository: HistoryRepository;
}
interface Repositories extends RepositoriesBase {
  init(): Promise<void>;
  withTransaction<T>(fn: (repos: RepositoryTransactionScope) => Promise<T>): Promise<T>;
}
type RepositoryTransactionScope = RepositoriesBase;
//#endregion
//#region logging/Logger.d.ts
type LogLevel = 'error' | 'warn' | 'info' | 'debug';
interface Logger {
  error(message: string, ...meta: unknown[]): void;
  warn(message: string, ...meta: unknown[]): void;
  info(message: string, ...meta: unknown[]): void;
  debug(message: string, ...meta: unknown[]): void;
  log?(level: LogLevel, message: string, ...meta: unknown[]): void;
  child?(bindings: Record<string, unknown>): Logger;
}
//#endregion
//#region infra/WsConnectionManager.d.ts
interface WebSocketLike {
  send(data: string): void;
  close(code?: number, reason?: string): void;
  addEventListener(type: 'open' | 'message' | 'error' | 'close', listener: (event: any) => void): void;
  removeEventListener(type: 'open' | 'message' | 'error' | 'close', listener: (event: any) => void): void;
}
type WebSocketFactory = (url: string) => WebSocketLike;
declare class WsConnectionManager {
  private readonly wsFactory;
  private readonly sockets;
  private readonly isOpenByMint;
  private readonly sendQueueByMint;
  private readonly logger?;
  private readonly listenersByMint;
  private readonly reconnectAttemptsByMint;
  private readonly reconnectTimeoutByMint;
  private paused;
  constructor(wsFactory: WebSocketFactory, logger?: Logger);
  private buildWsUrl;
  private ensureSocket;
  private scheduleReconnect;
  on(mintUrl: string, type: 'open' | 'message' | 'error' | 'close', listener: (event: any) => void): void;
  off(mintUrl: string, type: 'open' | 'message' | 'error' | 'close', listener: (event: any) => void): void;
  send(mintUrl: string, message: unknown): void;
  closeAll(): void;
  closeMint(mintUrl: string): void;
  pause(): void;
  resume(): void;
}
//#endregion
//#region infra/SubscriptionProtocol.d.ts
type JsonRpcId = number;
type WsRequestMethod = 'subscribe' | 'unsubscribe';
type SubscriptionKind = 'bolt11_mint_quote' | 'bolt11_melt_quote' | 'proof_state';
type UnsubscribeHandler = () => Promise<void>;
interface SubscribeParams {
  kind: SubscriptionKind;
  subId: string;
  filters: string[];
}
interface UnsubscribeParams {
  subId: string;
}
type WsRequest = {
  jsonrpc: '2.0';
  method: WsRequestMethod;
  params: SubscribeParams | UnsubscribeParams;
  id: JsonRpcId;
};
//#endregion
//#region infra/RealTimeTransport.d.ts
type TransportEvent = 'open' | 'message' | 'close' | 'error';
interface RealTimeTransport {
  on(mintUrl: string, event: TransportEvent, handler: (evt: any) => void): void;
  send(mintUrl: string, req: WsRequest): void;
  closeAll(): void;
  closeMint(mintUrl: string): void;
  pause(): void;
  resume(): void;
}
//#endregion
//#region infra/SubscriptionManager.d.ts
type SubscriptionCallback<TPayload = unknown> = (payload: TPayload) => void | Promise<void>;
declare class SubscriptionManager {
  private readonly nextIdByMint;
  private readonly subscriptions;
  private readonly activeByMint;
  private readonly pendingSubscribeByMint;
  private readonly transportByMint;
  private readonly logger?;
  private readonly messageHandlerByMint;
  private readonly openHandlerByMint;
  private readonly hasOpenedByMint;
  private readonly wsFactory?;
  private readonly capabilitiesProvider?;
  private paused;
  constructor(wsFactoryOrManager: WebSocketFactory | RealTimeTransport, logger?: Logger, capabilitiesProvider?: {
    getMintInfo: (mintUrl: string) => Promise<MintInfo>;
  });
  private getTransport;
  private isWebSocketAvailable;
  private getNextId;
  private ensureMessageListener;
  subscribe<TPayload = unknown>(mintUrl: string, kind: SubscriptionKind, filters: string[], onNotification?: SubscriptionCallback<TPayload>): Promise<{
    subId: string;
    unsubscribe: UnsubscribeHandler;
  }>;
  addCallback<TPayload = unknown>(subId: string, cb: SubscriptionCallback<TPayload>): void;
  removeCallback<TPayload = unknown>(subId: string, cb: SubscriptionCallback<TPayload>): void;
  unsubscribe(mintUrl: string, subId: string): Promise<void>;
  closeAll(): void;
  closeMint(mintUrl: string): void;
  private reSubscribeMint;
  private isMintWsSupported;
  pause(): void;
  resume(): void;
}
//#endregion
//#region infra/RequestRateLimiter.d.ts
type RequestFunction = <T>(options: {
  endpoint: string;
  requestBody?: Record<string, unknown>;
  headers?: Record<string, string>;
} & Omit<RequestInit, 'body' | 'headers'>) => Promise<T>;
interface RateLimiterOptions {
  capacity?: number;
  refillPerMinute?: number;
  bypassPathPrefixes?: string[];
  logger?: Logger;
}
/**
 * Token-bucket based request rate limiter that exposes a request-compatible API
 * for the cashu-ts `_customRequest` parameter.
 *
 * - Token capacity determines max burst size.
 * - Tokens refill continuously based on `refillPerMinute`.
 * - Paths starting with any configured prefix are not throttled.
 * - Requests are queued FIFO when tokens are exhausted.
 */
declare class RequestRateLimiter {
  private readonly capacity;
  private readonly refillPerMinute;
  private tokens;
  private lastRefillAt;
  private readonly bypassPathPrefixes;
  private readonly logger?;
  private queue;
  private processingTimer;
  constructor(options?: RateLimiterOptions);
  /**
   * The request function compatible with cashu-ts's `request(options)` signature.
   * It uses the global fetch under the hood.
   */
  request: RequestFunction;
  private shouldBypass;
  private performFetch;
  private acquireToken;
  private scheduleProcessingIfNeeded;
  private processQueue;
  private refillTokens;
  private msUntilNextToken;
}
//#endregion
//#region events/EventBus.d.ts
type EventHandler<Payload> = (payload: Payload) => void | Promise<void>;
type EventBusOptions<Events extends { [K in keyof Events]: unknown }> = {
  onError?: (args: {
    event: keyof Events;
    payload: Events[keyof Events];
    error: unknown;
  }) => void | Promise<void>;
  concurrency?: 'sequential' | 'parallel';
  throwOnError?: boolean;
};
type EmitOptions = {
  throwOnError?: boolean;
  failFast?: boolean;
};
declare class EventBus<Events extends { [K in keyof Events]: unknown }> {
  private readonly options;
  private listeners;
  constructor(options?: EventBusOptions<Events>);
  on<E extends keyof Events>(event: E, handler: EventHandler<Events[E]>): () => void;
  once<E extends keyof Events>(event: E, handler: EventHandler<Events[E]>): () => void;
  off<E extends keyof Events>(event: E, handler: EventHandler<Events[E]>): void;
  emit<E extends keyof Events>(event: E, payload: Events[E], options?: EmitOptions): Promise<void>;
}
//#endregion
//#region events/types.d.ts
interface CoreEvents {
  'mint:added': {
    mint: Mint;
    keysets: Keyset[];
  };
  'mint:updated': {
    mint: Mint;
    keysets: Keyset[];
  };
  'mint:trusted': {
    mintUrl: string;
  };
  'mint:untrusted': {
    mintUrl: string;
  };
  'counter:updated': Counter;
  'proofs:saved': {
    mintUrl: string;
    keysetId: string;
    proofs: CoreProof[];
  };
  'proofs:state-changed': {
    mintUrl: string;
    secrets: string[];
    state: ProofState;
  };
  'proofs:deleted': {
    mintUrl: string;
    secrets: string[];
  };
  'proofs:wiped': {
    mintUrl: string;
    keysetId: string;
  };
  'mint-quote:state-changed': {
    mintUrl: string;
    quoteId: string;
    state: MintQuoteState;
  };
  'mint-quote:created': {
    mintUrl: string;
    quoteId: string;
    quote: MintQuoteResponse;
  };
  'mint-quote:added': {
    mintUrl: string;
    quoteId: string;
    quote: MintQuoteResponse;
  };
  'mint-quote:requeue': {
    mintUrl: string;
    quoteId: string;
  };
  'mint-quote:redeemed': {
    mintUrl: string;
    quoteId: string;
    quote: MintQuoteResponse;
  };
  'melt-quote:created': {
    mintUrl: string;
    quoteId: string;
    quote: MeltQuoteResponse;
  };
  'melt-quote:state-changed': {
    mintUrl: string;
    quoteId: string;
    state: MeltQuoteState;
  };
  'melt-quote:paid': {
    mintUrl: string;
    quoteId: string;
    quote: MeltQuoteResponse;
  };
  'send:created': {
    mintUrl: string;
    token: Token;
  };
  'receive:created': {
    mintUrl: string;
    token: Token;
  };
  'history:updated': {
    mintUrl: string;
    entry: HistoryEntry;
  };
}
//#endregion
//#region logging/ConsoleLogger.d.ts
type ConsoleLoggerOptions = {
  level?: LogLevel;
};
declare class ConsoleLogger implements Logger {
  private prefix;
  private level;
  private static readonly levelPriority;
  constructor(prefix?: string, options?: ConsoleLoggerOptions);
  private shouldLog;
  error(message: string, ...meta: unknown[]): void;
  warn(message: string, ...meta: unknown[]): void;
  info(message: string, ...meta: unknown[]): void;
  debug(message: string, ...meta: unknown[]): void;
  log(level: LogLevel, message: string, ...meta: unknown[]): void;
  child(bindings: Record<string, unknown>): Logger;
}
//#endregion
//#region services/CounterService.d.ts
declare class CounterService {
  private readonly counterRepo;
  private readonly eventBus?;
  private readonly logger?;
  constructor(counterRepo: CounterRepository, logger?: Logger, eventBus?: EventBus<CoreEvents>);
  getCounter(mintUrl: string, keysetId: string): Promise<Counter>;
  incrementCounter(mintUrl: string, keysetId: string, n: number): Promise<{
    counter: number;
    mintUrl: string;
    keysetId: string;
  }>;
  overwriteCounter(mintUrl: string, keysetId: string, counter: number): Promise<{
    mintUrl: string;
    keysetId: string;
    counter: number;
  }>;
}
//#endregion
//#region services/SeedService.d.ts
declare class SeedService {
  private readonly seedGetter;
  private readonly seedTtlMs;
  private cachedSeed;
  private cachedUntil;
  private inFlight;
  constructor(seedGetter: () => Promise<Uint8Array>, options?: {
    seedTtlMs?: number;
  });
  getSeed(): Promise<Uint8Array>;
  clear(): void;
}
//#endregion
//#region services/KeyRingService.d.ts
declare class KeyRingService {
  private readonly logger?;
  private readonly keyRingRepository;
  private readonly seedService;
  constructor(keyRingRepository: KeyRingRepository, seedService: SeedService, logger?: Logger);
  generateNewKeyPair(): Promise<{
    publicKeyHex: string;
  }>;
  generateNewKeyPair(options: {
    dumpSecretKey: true;
  }): Promise<Keypair>;
  generateNewKeyPair(options: {
    dumpSecretKey: false;
  }): Promise<{
    publicKeyHex: string;
  }>;
  addKeyPair(secretKey: Uint8Array): Promise<Keypair>;
  removeKeyPair(publicKey: string): Promise<void>;
  getKeyPair(publicKey: string): Promise<Keypair | null>;
  getLatestKeyPair(): Promise<Keypair | null>;
  getAllKeyPairs(): Promise<Keypair[]>;
  signProof(proof: Proof, publicKey: string): Promise<Proof>;
  /**
   * Converts a secret key to its corresponding public key in SEC1 compressed format.
   * Note: schnorr.getPublicKey() returns a 32-byte x-only public key (BIP340).
   * We prepend '02' to create a 33-byte SEC1 compressed format as expected by Cashu.
   */
  private getPublicKeyHex;
}
//#endregion
//#region services/MintService.d.ts
declare class MintService {
  private readonly mintRepo;
  private readonly keysetRepo;
  private readonly mintAdapter;
  private readonly eventBus?;
  private readonly logger?;
  constructor(mintRepo: MintRepository, keysetRepo: KeysetRepository, logger?: Logger, eventBus?: EventBus<CoreEvents>);
  /**
   * Add a new mint by URL, running a single update cycle to fetch info & keysets.
   * If the mint already exists, it ensures it is updated.
   * New mints are added as untrusted by default unless explicitly specified.
   *
   * @param mintUrl - The URL of the mint to add
   * @param options - Optional configuration
   * @param options.trusted - Whether to add the mint as trusted (default: false)
   */
  addMintByUrl(mintUrl: string, options?: {
    trusted?: boolean;
  }): Promise<{
    mint: Mint;
    keysets: Keyset[];
  }>;
  updateMintData(mintUrl: string): Promise<{
    mint: Mint;
    keysets: Keyset[];
  }>;
  isTrustedMint(mintUrl: string): Promise<boolean>;
  ensureUpdatedMint(mintUrl: string): Promise<{
    mint: Mint;
    keysets: Keyset[];
  }>;
  deleteMint(mintUrl: string): Promise<void>;
  getMintInfo(mintUrl: string): Promise<MintInfo>;
  getAllMints(): Promise<Mint[]>;
  getAllTrustedMints(): Promise<Mint[]>;
  trustMint(mintUrl: string): Promise<void>;
  untrustMint(mintUrl: string): Promise<void>;
  private updateMint;
}
//#endregion
//#region services/WalletService.d.ts
declare class WalletService {
  private walletCache;
  private readonly CACHE_TTL;
  private readonly mintService;
  private readonly seedService;
  private inFlight;
  private readonly logger?;
  private readonly requestLimiters;
  private readonly requestLimiterOptionsForMint?;
  constructor(mintService: MintService, seedService: SeedService, logger?: Logger, requestLimiterOptionsForMint?: (mintUrl: string) => Partial<ConstructorParameters<typeof RequestRateLimiter>[0]>);
  getWallet(mintUrl: string): Promise<CashuWallet>;
  getWalletWithActiveKeysetId(mintUrl: string): Promise<{
    wallet: CashuWallet;
    keysetId: string;
    keyset: MintKeyset;
    keys: MintKeys;
  }>;
  /**
   * Clear cached wallet for a specific mint URL
   */
  clearCache(mintUrl: string): void;
  /**
   * Clear all cached wallets
   */
  clearAllCaches(): void;
  /**
   * Force refresh mint data and get fresh wallet
   */
  refreshWallet(mintUrl: string): Promise<CashuWallet>;
  private buildWallet;
  private getOrCreateRequestLimiter;
}
//#endregion
//#region services/ProofService.d.ts
declare class ProofService {
  private readonly counterService;
  private readonly proofRepository;
  private readonly eventBus?;
  private readonly walletService;
  private readonly keyRingService;
  private readonly seedService;
  private readonly logger?;
  constructor(counterService: CounterService, proofRepository: ProofRepository, walletService: WalletService, keyRingService: KeyRingService, seedService: SeedService, logger?: Logger, eventBus?: EventBus<CoreEvents>);
  /**
   * Calculates the send amount including receiver fees.
   * This is used when the sender pays fees for the receiver.
   */
  calculateSendAmountWithFees(mintUrl: string, sendAmount: number): Promise<number>;
  createOutputsAndIncrementCounters(mintUrl: string, amount: {
    keep: number;
    send: number;
  }, options?: {
    includeFees?: boolean;
  }): Promise<{
    keep: OutputData[];
    send: OutputData[];
    sendAmount: number;
    keepAmount: number;
  }>;
  saveProofs(mintUrl: string, proofs: CoreProof[]): Promise<void>;
  getReadyProofs(mintUrl: string): Promise<CoreProof[]>;
  getAllReadyProofs(): Promise<CoreProof[]>;
  /**
   * Gets the balance for a single mint by summing ready proof amounts.
   * @param mintUrl - The URL of the mint
   * @returns The total balance for the mint
   */
  getBalance(mintUrl: string): Promise<number>;
  /**
   * Gets balances for all mints by summing ready proof amounts.
   * @returns An object mapping mint URLs to their balances
   */
  getBalances(): Promise<{
    [mintUrl: string]: number;
  }>;
  setProofState(mintUrl: string, secrets: string[], state: 'inflight' | 'ready' | 'spent'): Promise<void>;
  deleteProofs(mintUrl: string, secrets: string[]): Promise<void>;
  wipeProofsByKeysetId(mintUrl: string, keysetId: string): Promise<void>;
  selectProofsToSend(mintUrl: string, amount: number, includeFees?: boolean): Promise<Proof[]>;
  private groupProofsByKeysetId;
  getProofsByKeysetId(mintUrl: string, keysetId: string): Promise<CoreProof[]>;
  hasProofsForKeyset(mintUrl: string, keysetId: string): Promise<boolean>;
  prepareProofsForReceiving(proofs: Proof[]): Promise<Proof[]>;
}
//#endregion
//#region services/MintQuoteService.d.ts
declare class MintQuoteService {
  private readonly mintQuoteRepo;
  private readonly mintService;
  private readonly walletService;
  private readonly proofService;
  private readonly eventBus;
  private readonly logger?;
  constructor(mintQuoteRepo: MintQuoteRepository, mintService: MintService, walletService: WalletService, proofService: ProofService, eventBus: EventBus<CoreEvents>, logger?: Logger);
  createMintQuote(mintUrl: string, amount: number): Promise<MintQuoteResponse>;
  redeemMintQuote(mintUrl: string, quoteId: string): Promise<void>;
  addExistingMintQuotes(mintUrl: string, quotes: MintQuoteResponse[]): Promise<{
    added: string[];
    skipped: string[];
  }>;
  updateStateFromRemote(mintUrl: string, quoteId: string, state: MintQuoteState): Promise<void>;
  private setMintQuoteState;
  /**
   * Requeue all PAID (but not yet ISSUED) quotes for processing.
   * Only requeues quotes for trusted mints.
   * Emits `mint-quote:requeue` for each PAID quote so the processor can enqueue them.
   */
  requeuePaidMintQuotes(mintUrl?: string): Promise<{
    requeued: string[];
  }>;
}
//#endregion
//#region services/watchers/MintQuoteWatcherService.d.ts
interface MintQuoteWatcherOptions {
  watchExistingPendingOnStart?: boolean;
}
declare class MintQuoteWatcherService {
  private readonly repo;
  private readonly subs;
  private readonly mintService;
  private readonly quotes;
  private readonly bus;
  private readonly logger?;
  private readonly options;
  private running;
  private unsubscribeByKey;
  private offCreated?;
  private offAdded?;
  private offUntrusted?;
  constructor(repo: MintQuoteRepository, subs: SubscriptionManager, mintService: MintService, quotes: MintQuoteService, bus: EventBus<CoreEvents>, logger?: Logger, options?: MintQuoteWatcherOptions);
  isRunning(): boolean;
  start(): Promise<void>;
  stop(): Promise<void>;
  watchQuote(mintUrl: string, quoteOrQuotes: string | string[]): Promise<void>;
  private stopWatching;
  stopWatchingMint(mintUrl: string): Promise<void>;
}
//#endregion
//#region services/watchers/MintQuoteProcessor.d.ts
interface QuoteHandler {
  canHandle(quoteType: string): boolean;
  process(mintUrl: string, quoteId: string): Promise<void>;
}
interface MintQuoteProcessorOptions {
  processIntervalMs?: number;
  maxRetries?: number;
  baseRetryDelayMs?: number;
  initialEnqueueDelayMs?: number;
}
declare class MintQuoteProcessor {
  private readonly quotes;
  private readonly bus;
  private readonly logger?;
  private running;
  private queue;
  private processing;
  private processingTimer?;
  private offStateChanged?;
  private offQuoteAdded?;
  private offRequeue?;
  private offUntrusted?;
  private handlers;
  private readonly processIntervalMs;
  private readonly maxRetries;
  private readonly baseRetryDelayMs;
  private readonly initialEnqueueDelayMs;
  constructor(quotes: MintQuoteService, bus: EventBus<CoreEvents>, logger?: Logger, options?: MintQuoteProcessorOptions);
  registerHandler(quoteType: string, handler: QuoteHandler): void;
  isRunning(): boolean;
  start(): Promise<void>;
  stop(): Promise<void>;
  /**
   * Wait for the queue to be empty and all processing to complete.
   * Useful for CLI applications that want to ensure all quotes are processed before exiting.
   */
  waitForCompletion(): Promise<void>;
  /**
   * Remove all queued items for a specific mint.
   * Called when a mint is untrusted to stop processing its quotes.
   */
  clearMintFromQueue(mintUrl: string): void;
  private enqueue;
  private scheduleNextProcess;
  private processNext;
  private processItem;
  private handleProcessingError;
  private updateQuoteState;
}
//#endregion
//#region services/watchers/ProofStateWatcherService.d.ts
interface ProofStateWatcherOptions {
  watchExistingInflightOnStart?: boolean;
}
declare class ProofStateWatcherService {
  private readonly subs;
  private readonly mintService;
  private readonly proofs;
  private readonly bus;
  private readonly logger?;
  private readonly options;
  private running;
  private unsubscribeByKey;
  private inflightByKey;
  private offProofsStateChanged?;
  private offUntrusted?;
  constructor(subs: SubscriptionManager, mintService: MintService, proofs: ProofService, bus: EventBus<CoreEvents>, logger?: Logger, options?: ProofStateWatcherOptions);
  isRunning(): boolean;
  start(): Promise<void>;
  stop(): Promise<void>;
  watchProof(mintUrl: string, secrets: string[]): Promise<void>;
  private stopWatching;
  stopWatchingMint(mintUrl: string): Promise<void>;
}
//#endregion
//#region services/WalletRestoreService.d.ts
declare class WalletRestoreService {
  private readonly proofService;
  private readonly counterService;
  private readonly walletService;
  private readonly logger?;
  private readonly restoreBatchSize;
  private readonly restoreGapLimit;
  private readonly restoreStartCounter;
  constructor(proofService: ProofService, counterService: CounterService, walletService: WalletService, logger?: Logger);
  sweepKeyset(mintUrl: string, keysetId: string, bip39seed: Uint8Array): Promise<void>;
  /**
   * Restore and persist proofs for a single keyset.
   * Enforces the invariant: restored proofs must be >= previously stored proofs.
   * Throws on any validation or persistence error. No transactions are used here.
   */
  restoreKeyset(mintUrl: string, wallet: CashuWallet, keysetId: string): Promise<void>;
}
//#endregion
//#region services/MeltQuoteService.d.ts
declare class MeltQuoteService {
  private readonly mintService;
  private readonly proofService;
  private readonly walletService;
  private readonly meltQuoteRepo;
  private readonly logger?;
  private readonly eventBus;
  constructor(mintService: MintService, proofService: ProofService, walletService: WalletService, meltQuoteRepo: MeltQuoteRepository, eventBus: EventBus<CoreEvents>, logger?: Logger);
  createMeltQuote(mintUrl: string, invoice: string): Promise<MeltQuoteResponse>;
  payMeltQuote(mintUrl: string, quoteId: string): Promise<void>;
  private setMeltQuoteState;
}
//#endregion
//#region services/HistoryService.d.ts
declare class HistoryService {
  private readonly historyRepository;
  private readonly logger?;
  private readonly eventBus;
  constructor(historyRepository: HistoryRepository, eventBus: EventBus<CoreEvents>, logger?: Logger);
  getPaginatedHistory(offset?: number, limit?: number): Promise<HistoryEntry[]>;
  handleSendCreated(mintUrl: string, token: Token): Promise<void>;
  handleReceiveCreated(mintUrl: string, token: Token): Promise<void>;
  handleMintQuoteStateChanged(mintUrl: string, quoteId: string, state: MintQuoteState): Promise<void>;
  handleMeltQuoteStateChanged(mintUrl: string, quoteId: string, state: MeltQuoteState): Promise<void>;
  handleMeltQuoteCreated(mintUrl: string, quoteId: string, quote: MeltQuoteResponse): Promise<void>;
  handleMintQuoteCreated(mintUrl: string, quoteId: string, quote: MintQuoteResponse): Promise<void>;
  handleMintQuoteAdded(mintUrl: string, quoteId: string, quote: MintQuoteResponse): Promise<void>;
  handleHistoryUpdated(mintUrl: string, entry: HistoryEntry): Promise<void>;
}
//#endregion
//#region services/TransactionService.d.ts
declare class TransactionService {
  private readonly mintService;
  private readonly walletService;
  private readonly proofService;
  private readonly eventBus;
  private readonly logger?;
  constructor(mintService: MintService, walletService: WalletService, proofService: ProofService, eventBus: EventBus<CoreEvents>, logger?: Logger);
  receive(token: Token | string): Promise<void>;
  send(mintUrl: string, amount: number): Promise<Token>;
}
//#endregion
//#region services/PaymentRequestService.d.ts
type InbandTransport = {
  type: 'inband';
};
type HttpTransport = {
  type: 'http';
  url: string;
};
type Transport = InbandTransport | HttpTransport;
type PreparedPaymentRequestBase = {
  mints?: string[];
};
type PreparedInbandPaymentRequest = PreparedPaymentRequestBase & {
  transport: InbandTransport;
  amount?: number;
};
type PreparedHttpPaymentRequest = PreparedPaymentRequestBase & {
  transport: HttpTransport;
  amount?: number;
};
type PreparedPaymentRequest = PreparedInbandPaymentRequest | PreparedHttpPaymentRequest;
declare class PaymentRequestService {
  private readonly transactionService;
  private readonly logger?;
  constructor(transactionService: TransactionService, logger?: Logger);
  readPaymentRequest(paymentRequest: string): Promise<PreparedPaymentRequest>;
  /**
   * Handle an inband payment request by sending tokens and calling the handler.
   * @param mintUrl - The mint to send from
   * @param request - The prepared payment request
   * @param inbandHandler - Callback to deliver the token
   * @param amount - Optional amount (required if not specified in request)
   */
  handleInbandPaymentRequest(mintUrl: string, request: PreparedInbandPaymentRequest, inbandHandler: (t: Token) => Promise<void>, amount?: number): Promise<void>;
  /**
   * Handle an HTTP payment request by sending tokens to the specified URL.
   * @param mintUrl - The mint to send from
   * @param request - The prepared payment request
   * @param amount - Optional amount (required if not specified in request)
   * @returns The HTTP response from the payment endpoint
   */
  handleHttpPaymentRequest(mintUrl: string, request: PreparedHttpPaymentRequest, amount?: number): Promise<Response>;
  private validateMint;
  private getPaymentRequestTransport;
  private validateAmount;
}
//#endregion
//#region api/WalletApi.d.ts
declare class WalletApi {
  private mintService;
  private walletService;
  private proofService;
  private walletRestoreService;
  private transactionService;
  private paymentRequestService;
  private readonly logger?;
  constructor(mintService: MintService, walletService: WalletService, proofService: ProofService, walletRestoreService: WalletRestoreService, transactionService: TransactionService, paymentRequestService: PaymentRequestService, logger?: Logger);
  receive(token: Token | string): Promise<void>;
  send(mintUrl: string, amount: number): Promise<Token>;
  getBalances(): Promise<{
    [mintUrl: string]: number;
  }>;
  /**
   * Parse and validate a payment request string.
   */
  readPaymentRequest(paymentRequest: string): Promise<PreparedPaymentRequest>;
  /**
   * Handle an inband payment request by sending tokens and calling the handler.
   * @param mintUrl - The mint to send from
   * @param request - The prepared payment request (from readPaymentRequest)
   * @param inbandHandler - Callback to deliver the token (e.g., display QR, send via NFC)
   * @param amount - Optional amount (required if not specified in request)
   */
  handleInbandPaymentRequest(mintUrl: string, request: PreparedPaymentRequest & {
    transport: {
      type: 'inband';
    };
  }, inbandHandler: (t: Token) => Promise<void>, amount?: number): Promise<void>;
  /**
   * Handle an HTTP payment request by sending tokens to the specified URL.
   * @param mintUrl - The mint to send from
   * @param request - The prepared payment request (from readPaymentRequest)
   * @param amount - Optional amount (required if not specified in request)
   * @returns The HTTP response from the payment endpoint
   */
  handleHttpPaymentRequest(mintUrl: string, request: PreparedPaymentRequest & {
    transport: {
      type: 'http';
      url: string;
    };
  }, amount?: number): Promise<Response>;
  restore(mintUrl: string): Promise<void>;
  /**
   * Sweeps a mint by sweeping each keyset and adds the swept proofs to the wallet
   * @param mintUrl - The URL of the mint to sweep
   * @param bip39seed - The BIP39 seed of the wallet to sweep
   */
  sweep(mintUrl: string, bip39seed: Uint8Array): Promise<void>;
}
//#endregion
//#region api/QuotesApi.d.ts
declare class QuotesApi {
  private mintQuoteService;
  private meltQuoteService;
  constructor(mintQuoteService: MintQuoteService, meltQuoteService: MeltQuoteService);
  createMintQuote(mintUrl: string, amount: number): Promise<MintQuoteResponse>;
  redeemMintQuote(mintUrl: string, quoteId: string): Promise<void>;
  createMeltQuote(mintUrl: string, invoice: string): Promise<MeltQuoteResponse>;
  payMeltQuote(mintUrl: string, quoteId: string): Promise<void>;
  addMintQuote(mintUrl: string, quotes: MintQuoteResponse[]): Promise<{
    added: string[];
    skipped: string[];
  }>;
  requeuePaidMintQuotes(mintUrl?: string): Promise<{
    requeued: string[];
  }>;
}
//#endregion
//#region models/Error.d.ts
declare class UnknownMintError extends Error {
  constructor(message: string);
}
declare class MintFetchError extends Error {
  readonly mintUrl: string;
  constructor(mintUrl: string, message?: string, cause?: unknown);
}
declare class KeysetSyncError extends Error {
  readonly mintUrl: string;
  readonly keysetId: string;
  constructor(mintUrl: string, keysetId: string, message?: string, cause?: unknown);
}
declare class ProofValidationError extends Error {
  constructor(message: string);
}
declare class ProofOperationError extends Error {
  readonly mintUrl: string;
  readonly keysetId?: string;
  constructor(mintUrl: string, message?: string, keysetId?: string, cause?: unknown);
}
/**
 * This error is thrown when a HTTP response is not 2XX nor a protocol error.
 */
declare class HttpResponseError extends Error {
  status: number;
  constructor(message: string, status: number);
}
/**
 * This error is thrown when a network request fails.
 */
declare class NetworkError extends Error {
  constructor(message: string);
}
/**
 * This error is thrown when a protocol error occurs per Cashu NUT-00 error codes.
 */
declare class MintOperationError extends HttpResponseError {
  code: number;
  constructor(code: number, detail: string);
}
/**
 * This error is thrown when a payment request is invalid or cannot be processed.
 */
declare class PaymentRequestError extends Error {
  constructor(message: string, cause?: unknown);
}
//#endregion
//#region api/MintApi.d.ts
declare class MintApi {
  private readonly mintService;
  constructor(mintService: MintService);
  addMint(mintUrl: string, options?: {
    trusted?: boolean;
  }): Promise<{
    mint: Mint;
    keysets: Keyset[];
  }>;
  getMintInfo(mintUrl: string): Promise<MintInfo>;
  isTrustedMint(mintUrl: string): Promise<boolean>;
  getAllMints(): Promise<Mint[]>;
  getAllTrustedMints(): Promise<Mint[]>;
  trustMint(mintUrl: string): Promise<void>;
  untrustMint(mintUrl: string): Promise<void>;
}
//#endregion
//#region api/KeyRingApi.d.ts
declare class KeyRingApi {
  private readonly keyRingService;
  constructor(keyRingService: KeyRingService);
  /**
   * Generates a new keypair and stores it in the keyring.
   * @param dumpSecretKey - If true, returns the full keypair including the secret key.
   *                        If false or omitted, returns only the public key.
   *                        WARNING: The secret key is sensitive cryptographic material. Handle with care.
   * @returns The full keypair (if dumpSecretKey is true) or just the public key (if false/omitted)
   */
  generateKeyPair(): Promise<{
    publicKeyHex: string;
  }>;
  generateKeyPair(dumpSecretKey: true): Promise<Keypair>;
  generateKeyPair(dumpSecretKey: false): Promise<{
    publicKeyHex: string;
  }>;
  /**
   * Adds an existing keypair to the keyring using a secret key.
   * @param secretKey - The 32-byte secret key as Uint8Array
   */
  addKeyPair(secretKey: Uint8Array): Promise<Keypair>;
  /**
   * Removes a keypair from the keyring.
   * @param publicKey - The public key (hex string) of the keypair to remove
   */
  removeKeyPair(publicKey: string): Promise<void>;
  /**
   * Retrieves a specific keypair by its public key.
   * @param publicKey - The public key (hex string) to look up
   * @returns The keypair if found, null otherwise
   */
  getKeyPair(publicKey: string): Promise<Keypair | null>;
  /**
   * Gets the most recently added keypair.
   * @returns The latest keypair if any exist, null otherwise
   */
  getLatestKeyPair(): Promise<Keypair | null>;
  /**
   * Gets all keypairs stored in the keyring.
   * @returns Array of all keypairs
   */
  getAllKeyPairs(): Promise<Keypair[]>;
}
//#endregion
//#region api/SubscriptionApi.d.ts
declare class SubscriptionApi {
  private readonly subs;
  private readonly logger?;
  constructor(subs: SubscriptionManager, logger?: Logger);
  awaitMintQuotePaid(mintUrl: string, quoteId: string): Promise<unknown>;
  awaitMeltQuotePaid(mintUrl: string, quoteId: string): Promise<unknown>;
  private awaitFirstNotification;
}
//#endregion
//#region api/HistoryApi.d.ts
declare class HistoryApi {
  private historyService;
  constructor(historyService: HistoryService);
  getPaginatedHistory(offset?: number, limit?: number): Promise<HistoryEntry[]>;
}
//#endregion
//#region plugins/types.d.ts
type ServiceKey = 'mintService' | 'walletService' | 'proofService' | 'keyRingService' | 'seedService' | 'walletRestoreService' | 'counterService' | 'mintQuoteService' | 'meltQuoteService' | 'historyService' | 'transactionService' | 'subscriptions' | 'eventBus' | 'logger';
interface ServiceMap {
  mintService: MintService;
  walletService: WalletService;
  proofService: ProofService;
  keyRingService: KeyRingService;
  seedService: SeedService;
  walletRestoreService: WalletRestoreService;
  counterService: CounterService;
  mintQuoteService: MintQuoteService;
  meltQuoteService: MeltQuoteService;
  historyService: HistoryService;
  transactionService: TransactionService;
  subscriptions: SubscriptionManager;
  eventBus: EventBus<CoreEvents>;
  logger: Logger;
}
interface PluginContext<Req extends readonly ServiceKey[] = readonly ServiceKey[]> {
  services: Pick<ServiceMap, Req[number]>;
}
type CleanupFn = () => void | Promise<void>;
type Cleanup = void | CleanupFn | Promise<void | CleanupFn>;
interface Plugin<Req extends readonly ServiceKey[] = readonly ServiceKey[]> {
  name: string;
  required: Req;
  optional?: readonly ServiceKey[];
  onInit?(ctx: PluginContext<Req>): Cleanup;
  onReady?(ctx: PluginContext<Req>): Cleanup;
  onDispose?(): void | Promise<void>;
}
//#endregion
//#region Manager.d.ts
/**
 * Configuration options for initializing the Coco Cashu manager
 */
interface CocoConfig {
  /** Repository implementations for data persistence */
  repo: Repositories;
  /** Function that returns the wallet seed as Uint8Array */
  seedGetter: () => Promise<Uint8Array>;
  /** Optional logger instance (defaults to NullLogger) */
  logger?: Logger;
  /** Optional WebSocket factory for real-time subscriptions */
  webSocketFactory?: WebSocketFactory;
  /** Optional plugins to extend functionality */
  plugins?: Plugin[];
  /**
   * Watcher configuration (all enabled by default)
   * - Omit to use defaults (enabled)
   * - Set `disabled: true` to disable
   * - Provide options to customize behavior
   */
  watchers?: {
    /** Mint quote watcher (enabled by default) */
    mintQuoteWatcher?: {
      disabled?: boolean;
      watchExistingPendingOnStart?: boolean;
    };
    /** Proof state watcher (enabled by default) */
    proofStateWatcher?: {
      disabled?: boolean;
    };
  };
  /**
   * Processor configuration (all enabled by default)
   * - Omit to use defaults (enabled)
   * - Set `disabled: true` to disable
   * - Provide options to customize behavior
   */
  processors?: {
    /** Mint quote processor (enabled by default) */
    mintQuoteProcessor?: {
      disabled?: boolean;
      processIntervalMs?: number;
      maxRetries?: number;
      baseRetryDelayMs?: number;
      initialEnqueueDelayMs?: number;
    };
  };
}
/**
 * Initializes and configures a new Coco Cashu manager instance
 * @param config - Configuration options including repositories, seed, and optional features
 * @returns A fully initialized Manager instance
 */
declare function initializeCoco(config: CocoConfig): Promise<Manager>;
declare class Manager {
  readonly mint: MintApi;
  readonly wallet: WalletApi;
  readonly quotes: QuotesApi;
  readonly keyring: KeyRingApi;
  readonly subscription: SubscriptionApi;
  readonly history: HistoryApi;
  private mintService;
  private walletService;
  private proofService;
  private walletRestoreService;
  private keyRingService;
  private eventBus;
  private logger;
  readonly subscriptions: SubscriptionManager;
  private mintQuoteService;
  private mintQuoteWatcher?;
  private mintQuoteProcessor?;
  private mintQuoteRepository;
  private proofStateWatcher?;
  private meltQuoteService;
  private historyService;
  private seedService;
  private counterService;
  private transactionService;
  private paymentRequestService;
  private readonly pluginHost;
  private subscriptionsPaused;
  private originalWatcherConfig;
  private originalProcessorConfig;
  constructor(repositories: Repositories, seedGetter: () => Promise<Uint8Array>, logger?: Logger, webSocketFactory?: WebSocketFactory, plugins?: Plugin[], watchers?: CocoConfig['watchers'], processors?: CocoConfig['processors']);
  on<E extends keyof CoreEvents>(event: E, handler: (payload: CoreEvents[E]) => void | Promise<void>): () => void;
  once<E extends keyof CoreEvents>(event: E, handler: (payload: CoreEvents[E]) => void | Promise<void>): () => void;
  use(plugin: Plugin): void;
  dispose(): Promise<void>;
  off<E extends keyof CoreEvents>(event: E, handler: (payload: CoreEvents[E]) => void | Promise<void>): void;
  enableMintQuoteWatcher(options?: {
    watchExistingPendingOnStart?: boolean;
  }): Promise<void>;
  disableMintQuoteWatcher(): Promise<void>;
  enableMintQuoteProcessor(options?: {
    processIntervalMs?: number;
    maxRetries?: number;
    baseRetryDelayMs?: number;
    initialEnqueueDelayMs?: number;
  }): Promise<boolean>;
  disableMintQuoteProcessor(): Promise<void>;
  waitForMintQuoteProcessor(): Promise<void>;
  enableProofStateWatcher(): Promise<void>;
  disableProofStateWatcher(): Promise<void>;
  pauseSubscriptions(): Promise<void>;
  resumeSubscriptions(): Promise<void>;
  private getChildLogger;
  private createEventBus;
  private createSubscriptionManager;
  private buildCoreServices;
  private buildApis;
}
//#endregion
//#region plugins/PluginHost.d.ts
declare class PluginHost {
  private readonly plugins;
  private readonly cleanups;
  private services?;
  private initialized;
  private readyPhase;
  use(plugin: Plugin): void;
  init(services: ServiceMap): Promise<void>;
  ready(): Promise<void>;
  dispose(): Promise<void>;
  private runInit;
  private runReady;
  private createContext;
}
//#endregion
//#region utils.d.ts
/**
 * Normalize a mint URL to prevent duplicates from variations like:
 * - Trailing slashes: https://mint.com/ -> https://mint.com
 * - Case differences in hostname: https://MINT.com -> https://mint.com
 * - Default ports: https://mint.com:443 -> https://mint.com
 * - Redundant path segments: https://mint.com/./path -> https://mint.com/path
 */
declare function normalizeMintUrl(mintUrl: string): string;
//#endregion
export { Cleanup, CleanupFn, CocoConfig, ConsoleLogger, type CoreProof, Counter, CounterRepository, CounterService, HistoryApi, HistoryEntry, HistoryRepository, HistoryService, HttpResponseError, type HttpTransport, type InbandTransport, KeyRingApi, KeyRingRepository, KeyRingService, Keypair, Keyset, KeysetRepository, KeysetSyncError, type Logger, Manager, MeltHistoryEntry, MeltQuote, MeltQuoteRepository, MeltQuoteService, MemoryCounterRepository, MemoryHistoryRepository, MemoryKeyRingRepository, MemoryKeysetRepository, MemoryMeltQuoteRepository, MemoryMintQuoteRepository, MemoryMintRepository, MemoryProofRepository, MemoryRepositories, Mint, MintApi, MintFetchError, MintHistoryEntry, MintOperationError, MintQuote, MintQuoteProcessor, MintQuoteProcessorOptions, MintQuoteRepository, MintQuoteService, MintQuoteWatcherOptions, MintQuoteWatcherService, MintRepository, MintService, NetworkError, PaymentRequestError, PaymentRequestService, Plugin, PluginContext, PluginHost, type PreparedHttpPaymentRequest, type PreparedInbandPaymentRequest, type PreparedPaymentRequest, ProofOperationError, ProofRepository, ProofService, type ProofState, ProofStateWatcherOptions, ProofStateWatcherService, ProofValidationError, QuotesApi, ReceiveHistoryEntry, Repositories, RepositoryTransactionScope, SeedService, SendHistoryEntry, ServiceKey, ServiceMap, SubscriptionApi, SubscriptionManager, TransactionService, type Transport, UnknownMintError, WalletApi, WalletRestoreService, WalletService, type WebSocketFactory, type WebSocketLike, WsConnectionManager, getDecodedToken, getEncodedToken, initializeCoco, normalizeMintUrl };