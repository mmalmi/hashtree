//#region rolldown:runtime
var __create = Object.create;
var __defProp = Object.defineProperty;
var __getOwnPropDesc = Object.getOwnPropertyDescriptor;
var __getOwnPropNames = Object.getOwnPropertyNames;
var __getProtoOf = Object.getPrototypeOf;
var __hasOwnProp = Object.prototype.hasOwnProperty;
var __copyProps = (to, from, except, desc) => {
	if (from && typeof from === "object" || typeof from === "function") for (var keys = __getOwnPropNames(from), i = 0, n = keys.length, key; i < n; i++) {
		key = keys[i];
		if (!__hasOwnProp.call(to, key) && key !== except) __defProp(to, key, {
			get: ((k) => from[k]).bind(null, key),
			enumerable: !(desc = __getOwnPropDesc(from, key)) || desc.enumerable
		});
	}
	return to;
};
var __toESM = (mod, isNodeMode, target) => (target = mod != null ? __create(__getProtoOf(mod)) : {}, __copyProps(isNodeMode || !mod || !mod.__esModule ? __defProp(target, "default", {
	value: mod,
	enumerable: true
}) : target, mod));

//#endregion
let __cashu_cashu_ts_crypto_common = require("@cashu/cashu-ts/crypto/common");
__cashu_cashu_ts_crypto_common = __toESM(__cashu_cashu_ts_crypto_common);
let __noble_curves_secp256k1_js = require("@noble/curves/secp256k1.js");
__noble_curves_secp256k1_js = __toESM(__noble_curves_secp256k1_js);
let __noble_curves_utils_js = require("@noble/curves/utils.js");
__noble_curves_utils_js = __toESM(__noble_curves_utils_js);
let __noble_hashes_sha2_js = require("@noble/hashes/sha2.js");
__noble_hashes_sha2_js = __toESM(__noble_hashes_sha2_js);
let __scure_bip32 = require("@scure/bip32");
__scure_bip32 = __toESM(__scure_bip32);
let __cashu_cashu_ts = require("@cashu/cashu-ts");
__cashu_cashu_ts = __toESM(__cashu_cashu_ts);

//#region events/EventBus.ts
var EventBus = class {
	listeners = /* @__PURE__ */ new Map();
	constructor(options = {}) {
		this.options = options;
	}
	on(event, handler) {
		let set = this.listeners.get(event);
		if (!set) {
			set = /* @__PURE__ */ new Set();
			this.listeners.set(event, set);
		}
		set.add(handler);
		return () => this.off(event, handler);
	}
	once(event, handler) {
		const wrapped = async (payload) => {
			this.off(event, wrapped);
			await handler(payload);
		};
		return this.on(event, wrapped);
	}
	off(event, handler) {
		const set = this.listeners.get(event);
		if (!set) return;
		set.delete(handler);
		if (set.size === 0) this.listeners.delete(event);
	}
	async emit(event, payload, options) {
		const set = this.listeners.get(event);
		if (!set || set.size === 0) return;
		const handlers = Array.from(set);
		const effectiveThrow = options?.throwOnError ?? this.options.throwOnError ?? false;
		if ((this.options.concurrency ?? "sequential") === "parallel") {
			const results = await Promise.allSettled(handlers.map((h) => h(payload)));
			const errors = [];
			for (const r of results) if (r.status === "rejected") {
				errors.push(r.reason);
				if (this.options.onError) await this.options.onError({
					event,
					payload,
					error: r.reason
				});
			}
			if (errors.length && effectiveThrow) throw new AggregateError(errors, `Event "${String(event)}" had ${errors.length} handler error(s)`);
			return;
		}
		const collectedErrors = [];
		for (const handler of handlers) try {
			await handler(payload);
		} catch (error) {
			if (this.options.onError) await this.options.onError({
				event,
				payload,
				error
			});
			if (effectiveThrow && options?.failFast) throw error;
			if (effectiveThrow) collectedErrors.push(error);
		}
		if (collectedErrors.length && effectiveThrow) throw new AggregateError(collectedErrors, `Event "${String(event)}" had ${collectedErrors.length} handler error(s)`);
	}
};

//#endregion
//#region utils.ts
function mapProofToCoreProof(mintUrl, state, proofs) {
	return proofs.map((p) => ({
		...p,
		mintUrl,
		state
	}));
}
function assertNonNegativeInteger(paramName, value, logger) {
	if (!Number.isFinite(value) || !Number.isInteger(value) || value < 0) {
		logger?.warn("Invalid numeric value", { [paramName]: value });
		throw new Error(`${paramName} must be a non-negative integer`);
	}
}
function toBase64Url(bytes) {
	let base64;
	const Buf = globalThis.Buffer;
	if (typeof Buf !== "undefined") base64 = Buf.from(bytes).toString("base64");
	else if (typeof btoa !== "undefined") {
		let bin = "";
		for (const b of bytes) bin += String.fromCharCode(b);
		base64 = btoa(bin);
	}
	if (!base64) return Array.from(bytes).map((b) => b.toString(16).padStart(2, "0")).join("");
	return base64.replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}
function generateSubId() {
	const length = 16;
	const bytes = new Uint8Array(length);
	const cryptoObj = globalThis.crypto;
	if (cryptoObj && typeof cryptoObj.getRandomValues === "function") cryptoObj.getRandomValues(bytes);
	else for (let i = 0; i < length; i++) bytes[i] = Math.floor(Math.random() * 256);
	return toBase64Url(bytes);
}
/**
* Compute the Y point (hex, compressed) for a single secret using hash-to-curve.
*/
function computeYHexForSecrets(secrets) {
	const encoder = new TextEncoder();
	return secrets.map((secret) => (0, __cashu_cashu_ts_crypto_common.hashToCurve)(encoder.encode(secret)).toHex(true));
}
/**
* Build bidirectional maps between secrets and their Y points (hex) using hash-to-curve.
* - yHexBySecret: secret -> Y hex
* - secretByYHex: Y hex -> secret
*/
function buildYHexMapsForSecrets(secrets) {
	const yHexBySecret = /* @__PURE__ */ new Map();
	const secretByYHex = /* @__PURE__ */ new Map();
	const yHexes = computeYHexForSecrets(secrets);
	for (let i = 0; i < secrets.length; i++) {
		const secret = secrets[i];
		const yHex = yHexes[i];
		if (!secret || !yHex) continue;
		yHexBySecret.set(secret, yHex);
		secretByYHex.set(yHex, secret);
	}
	return {
		yHexBySecret,
		secretByYHex
	};
}
/**
* Normalize a mint URL to prevent duplicates from variations like:
* - Trailing slashes: https://mint.com/ -> https://mint.com
* - Case differences in hostname: https://MINT.com -> https://mint.com
* - Default ports: https://mint.com:443 -> https://mint.com
* - Redundant path segments: https://mint.com/./path -> https://mint.com/path
*/
function normalizeMintUrl(mintUrl) {
	const url = new URL(mintUrl);
	if (url.protocol === "https:" && url.port === "443" || url.protocol === "http:" && url.port === "80") url.port = "";
	let normalized = `${url.protocol}//${url.host}${url.pathname}`;
	if (normalized.endsWith("/") && url.pathname !== "/") normalized = normalized.slice(0, -1);
	else if (url.pathname === "/") normalized = `${url.protocol}//${url.host}`;
	return normalized;
}

//#endregion
//#region services/CounterService.ts
var CounterService = class {
	counterRepo;
	eventBus;
	logger;
	constructor(counterRepo, logger, eventBus) {
		this.counterRepo = counterRepo;
		this.logger = logger;
		this.eventBus = eventBus;
	}
	async getCounter(mintUrl, keysetId) {
		const counter = await this.counterRepo.getCounter(mintUrl, keysetId);
		if (!counter) {
			const newCounter = {
				mintUrl,
				keysetId,
				counter: 0
			};
			await this.counterRepo.setCounter(mintUrl, keysetId, 0);
			this.logger?.debug("Initialized counter", {
				mintUrl,
				keysetId
			});
			return newCounter;
		}
		return counter;
	}
	async incrementCounter(mintUrl, keysetId, n) {
		assertNonNegativeInteger("n", n, this.logger);
		const current = await this.getCounter(mintUrl, keysetId);
		const updatedValue = current.counter + n;
		await this.counterRepo.setCounter(mintUrl, keysetId, updatedValue);
		const updated = {
			...current,
			counter: updatedValue
		};
		await this.eventBus?.emit("counter:updated", updated);
		this.logger?.info("Counter incremented", {
			mintUrl,
			keysetId,
			counter: updatedValue
		});
		return updated;
	}
	async overwriteCounter(mintUrl, keysetId, counter) {
		assertNonNegativeInteger("counter", counter, this.logger);
		await this.counterRepo.setCounter(mintUrl, keysetId, counter);
		const updated = {
			mintUrl,
			keysetId,
			counter
		};
		await this.eventBus?.emit("counter:updated", updated);
		this.logger?.info("Counter overwritten", {
			mintUrl,
			keysetId,
			counter
		});
		return updated;
	}
};

//#endregion
//#region services/KeyRingService.ts
var KeyRingService = class {
	logger;
	keyRingRepository;
	seedService;
	constructor(keyRingRepository, seedService, logger) {
		this.keyRingRepository = keyRingRepository;
		this.logger = logger;
		this.seedService = seedService;
	}
	async generateNewKeyPair(options) {
		this.logger?.debug("Generating new key pair");
		const nextDerivationIndex = await this.keyRingRepository.getLastDerivationIndex() + 1;
		const seed = await this.seedService.getSeed();
		const hdKey = __scure_bip32.HDKey.fromMasterSeed(seed);
		const derivationPath = `m/129373'/10'/0'/0'/${nextDerivationIndex}`;
		const { privateKey: secretKey } = hdKey.derive(derivationPath);
		if (!secretKey) throw new Error("Failed to derive secret key");
		const publicKeyHex = this.getPublicKeyHex(secretKey);
		await this.keyRingRepository.setPersistedKeyPair({
			publicKeyHex,
			secretKey,
			derivationIndex: nextDerivationIndex
		});
		this.logger?.debug("New key pair generated", { publicKeyHex });
		if (options?.dumpSecretKey) return {
			publicKeyHex,
			secretKey
		};
		return { publicKeyHex };
	}
	async addKeyPair(secretKey) {
		this.logger?.debug("Adding key pair with secret key...");
		if (secretKey.length !== 32) throw new Error("Secret key must be exactly 32 bytes");
		const publicKeyHex = this.getPublicKeyHex(secretKey);
		await this.keyRingRepository.setPersistedKeyPair({
			publicKeyHex,
			secretKey
		});
		this.logger?.debug("Key pair added", { publicKeyHex });
		return {
			publicKeyHex,
			secretKey
		};
	}
	async removeKeyPair(publicKey) {
		this.logger?.debug("Removing key pair", { publicKey });
		await this.keyRingRepository.deletePersistedKeyPair(publicKey);
		this.logger?.debug("Key pair removed", { publicKey });
	}
	async getKeyPair(publicKey) {
		if (!publicKey || typeof publicKey !== "string") throw new Error("Public key is required and must be a string");
		return this.keyRingRepository.getPersistedKeyPair(publicKey);
	}
	async getLatestKeyPair() {
		return this.keyRingRepository.getLatestKeyPair();
	}
	async getAllKeyPairs() {
		return this.keyRingRepository.getAllPersistedKeyPairs();
	}
	async signProof(proof, publicKey) {
		this.logger?.debug("Signing proof", {
			proof,
			publicKey
		});
		if (!proof.secret || typeof proof.secret !== "string") throw new Error("Proof secret is required and must be a string");
		const keyPair = await this.keyRingRepository.getPersistedKeyPair(publicKey);
		if (!keyPair) {
			const publicKeyPreview = publicKey.substring(0, 8);
			this.logger?.error("Key pair not found", { publicKey });
			throw new Error(`Key pair not found for public key: ${publicKeyPreview}...`);
		}
		const message = new TextEncoder().encode(proof.secret);
		const signature = __noble_curves_secp256k1_js.schnorr.sign((0, __noble_hashes_sha2_js.sha256)(message), keyPair.secretKey);
		const signedProof = {
			...proof,
			witness: JSON.stringify({ signatures: [(0, __noble_curves_utils_js.bytesToHex)(signature)] })
		};
		this.logger?.debug("Proof signed successfully", { publicKey });
		return signedProof;
	}
	/**
	* Converts a secret key to its corresponding public key in SEC1 compressed format.
	* Note: schnorr.getPublicKey() returns a 32-byte x-only public key (BIP340).
	* We prepend '02' to create a 33-byte SEC1 compressed format as expected by Cashu.
	*/
	getPublicKeyHex(secretKey) {
		const publicKey = __noble_curves_secp256k1_js.schnorr.getPublicKey(secretKey);
		return "02" + (0, __noble_curves_utils_js.bytesToHex)(publicKey);
	}
};

//#endregion
//#region models/Error.ts
var UnknownMintError = class extends Error {
	constructor(message) {
		super(message);
		this.name = "UnknownMintError";
	}
};
var MintFetchError = class extends Error {
	mintUrl;
	constructor(mintUrl, message, cause) {
		super(message ?? `Failed to fetch mint ${mintUrl}`);
		this.name = "MintFetchError";
		this.mintUrl = mintUrl;
		this.cause = cause;
	}
};
var KeysetSyncError = class extends Error {
	mintUrl;
	keysetId;
	constructor(mintUrl, keysetId, message, cause) {
		super(message ?? `Failed to sync keyset ${keysetId} for mint ${mintUrl}`);
		this.name = "KeysetSyncError";
		this.mintUrl = mintUrl;
		this.keysetId = keysetId;
		this.cause = cause;
	}
};
var ProofValidationError = class extends Error {
	constructor(message) {
		super(message);
		this.name = "ProofValidationError";
	}
};
var ProofOperationError = class extends Error {
	mintUrl;
	keysetId;
	constructor(mintUrl, message, keysetId, cause) {
		super(message ?? `Proof operation failed for mint ${mintUrl}${keysetId ? ` keyset ${keysetId}` : ""}`);
		this.name = "ProofOperationError";
		this.mintUrl = mintUrl;
		this.keysetId = keysetId;
		this.cause = cause;
	}
};
/**
* This error is thrown when a HTTP response is not 2XX nor a protocol error.
*/
var HttpResponseError = class HttpResponseError extends Error {
	status;
	constructor(message, status) {
		super(message);
		this.status = status;
		this.name = "HttpResponseError";
		Object.setPrototypeOf(this, HttpResponseError.prototype);
	}
};
/**
* This error is thrown when a network request fails.
*/
var NetworkError = class NetworkError extends Error {
	constructor(message) {
		super(message);
		this.name = "NetworkError";
		Object.setPrototypeOf(this, NetworkError.prototype);
	}
};
/**
* This error is thrown when a protocol error occurs per Cashu NUT-00 error codes.
*/
var MintOperationError = class MintOperationError extends HttpResponseError {
	code;
	constructor(code, detail) {
		super(detail || "Unknown mint operation error", 400);
		this.code = code;
		this.name = "MintOperationError";
		Object.setPrototypeOf(this, MintOperationError.prototype);
	}
};
/**
* This error is thrown when a payment request is invalid or cannot be processed.
*/
var PaymentRequestError = class extends Error {
	constructor(message, cause) {
		super(message);
		this.name = "PaymentRequestError";
		this.cause = cause;
	}
};

//#endregion
//#region infra/MintAdapter.ts
var MintAdapter = class {
	cashuMints = {};
	async fetchMintInfo(mintUrl) {
		return await (await this.getCashuMint(mintUrl)).getInfo();
	}
	async fetchKeysets(mintUrl) {
		return await (await this.getCashuMint(mintUrl)).getKeySets();
	}
	async fetchKeysForId(mintUrl, id) {
		const { keysets } = await (await this.getCashuMint(mintUrl)).getKeys(id);
		if (keysets.length !== 1 || !keysets[0]) throw new Error(`Expected 1 keyset for ${id}, got ${keysets.length}`);
		return keysets[0].keys;
	}
	async getCashuMint(mintUrl) {
		if (!this.cashuMints[mintUrl]) this.cashuMints[mintUrl] = new __cashu_cashu_ts.CashuMint(mintUrl);
		return this.cashuMints[mintUrl];
	}
	async checkMintQuoteState(_mintUrl, _quoteId) {
		return {};
	}
	async checkMeltQuoteState(_mintUrl, _quoteId) {
		return {};
	}
	async checkProofStates(_mintUrl, _proofSecrets) {
		return [];
	}
};

//#endregion
//#region services/MintService.ts
const MINT_REFRESH_TTL_S = 300;
var MintService = class {
	mintRepo;
	keysetRepo;
	mintAdapter;
	eventBus;
	logger;
	constructor(mintRepo, keysetRepo, logger, eventBus) {
		this.mintRepo = mintRepo;
		this.keysetRepo = keysetRepo;
		this.mintAdapter = new MintAdapter();
		this.logger = logger;
		this.eventBus = eventBus;
	}
	/**
	* Add a new mint by URL, running a single update cycle to fetch info & keysets.
	* If the mint already exists, it ensures it is updated.
	* New mints are added as untrusted by default unless explicitly specified.
	*
	* @param mintUrl - The URL of the mint to add
	* @param options - Optional configuration
	* @param options.trusted - Whether to add the mint as trusted (default: false)
	*/
	async addMintByUrl(mintUrl, options) {
		mintUrl = normalizeMintUrl(mintUrl);
		const trusted = options?.trusted ?? false;
		this.logger?.info("Adding mint by URL", {
			mintUrl,
			trusted
		});
		const exists = await this.mintRepo.getMintByUrl(mintUrl).catch(() => null);
		if (exists) {
			if (options?.trusted !== void 0 && exists.trusted !== options.trusted) {
				await this.mintRepo.setMintTrusted(mintUrl, options.trusted);
				this.logger?.info("Updated mint trust status", {
					mintUrl,
					trusted: options.trusted
				});
			}
			return this.ensureUpdatedMint(mintUrl);
		}
		const now = Math.floor(Date.now() / 1e3);
		const newMint = {
			mintUrl,
			name: mintUrl,
			mintInfo: {},
			trusted,
			createdAt: now,
			updatedAt: 0
		};
		const added = await this.updateMint(newMint);
		await this.eventBus?.emit("mint:added", added);
		this.logger?.info("Mint added", {
			mintUrl,
			trusted
		});
		return added;
	}
	async updateMintData(mintUrl) {
		mintUrl = normalizeMintUrl(mintUrl);
		const mint = await this.mintRepo.getMintByUrl(mintUrl).catch(() => null);
		if (!mint) {
			const now = Math.floor(Date.now() / 1e3);
			const newMint = {
				mintUrl,
				name: mintUrl,
				mintInfo: {},
				trusted: false,
				createdAt: now,
				updatedAt: 0
			};
			return this.updateMint(newMint);
		}
		return this.updateMint(mint);
	}
	async isTrustedMint(mintUrl) {
		return await this.mintRepo.isTrustedMint(normalizeMintUrl(mintUrl));
	}
	async ensureUpdatedMint(mintUrl) {
		mintUrl = normalizeMintUrl(mintUrl);
		let mint = await this.mintRepo.getMintByUrl(mintUrl).catch(() => null);
		if (!mint) {
			const now$1 = Math.floor(Date.now() / 1e3);
			mint = {
				mintUrl,
				name: mintUrl,
				mintInfo: {},
				trusted: false,
				createdAt: now$1,
				updatedAt: 0
			};
		}
		const now = Math.floor(Date.now() / 1e3);
		if (mint.updatedAt < now - MINT_REFRESH_TTL_S) {
			this.logger?.debug("Refreshing stale mint", { mintUrl });
			const updated = await this.updateMint(mint);
			await this.eventBus?.emit("mint:updated", updated);
			return updated;
		}
		const keysets = await this.keysetRepo.getKeysetsByMintUrl(mint.mintUrl);
		return {
			mint,
			keysets
		};
	}
	async deleteMint(mintUrl) {
		mintUrl = normalizeMintUrl(mintUrl);
		if (!await this.mintRepo.getMintByUrl(mintUrl).catch(() => null)) return;
		const keysets = await this.keysetRepo.getKeysetsByMintUrl(mintUrl);
		await Promise.all(keysets.map((ks) => this.keysetRepo.deleteKeyset(mintUrl, ks.id)));
		await this.mintRepo.deleteMint(mintUrl);
	}
	async getMintInfo(mintUrl) {
		const { mint } = await this.ensureUpdatedMint(normalizeMintUrl(mintUrl));
		return mint.mintInfo;
	}
	async getAllMints() {
		return await this.mintRepo.getAllMints();
	}
	async getAllTrustedMints() {
		return await this.mintRepo.getAllTrustedMints();
	}
	async trustMint(mintUrl) {
		mintUrl = normalizeMintUrl(mintUrl);
		this.logger?.info("Trusting mint", { mintUrl });
		await this.mintRepo.setMintTrusted(mintUrl, true);
		await this.eventBus?.emit("mint:trusted", { mintUrl });
		await this.eventBus?.emit("mint:updated", await this.ensureUpdatedMint(mintUrl));
	}
	async untrustMint(mintUrl) {
		mintUrl = normalizeMintUrl(mintUrl);
		this.logger?.info("Untrusting mint", { mintUrl });
		await this.mintRepo.setMintTrusted(mintUrl, false);
		await this.eventBus?.emit("mint:untrusted", { mintUrl });
		await this.eventBus?.emit("mint:updated", await this.ensureUpdatedMint(mintUrl));
	}
	async updateMint(mint) {
		let mintInfo;
		try {
			this.logger?.debug("Fetching mint info", { mintUrl: mint.mintUrl });
			mintInfo = await this.mintAdapter.fetchMintInfo(mint.mintUrl);
		} catch (err) {
			this.logger?.error("Failed to fetch mint info", {
				mintUrl: mint.mintUrl,
				err
			});
			throw new MintFetchError(mint.mintUrl, void 0, err);
		}
		let keysets;
		try {
			this.logger?.debug("Fetching keysets", { mintUrl: mint.mintUrl });
			({keysets} = await this.mintAdapter.fetchKeysets(mint.mintUrl));
		} catch (err) {
			this.logger?.error("Failed to fetch keysets", {
				mintUrl: mint.mintUrl,
				err
			});
			throw new MintFetchError(mint.mintUrl, "Failed to fetch keysets", err);
		}
		await Promise.all(keysets.map(async (ks) => {
			if (await this.keysetRepo.getKeysetById(mint.mintUrl, ks.id)) {
				const keysetModel = {
					mintUrl: mint.mintUrl,
					id: ks.id,
					unit: ks.unit,
					active: ks.active,
					feePpk: ks.input_fee_ppk || 0
				};
				return this.keysetRepo.updateKeyset(keysetModel);
			} else try {
				const keysRes = await this.mintAdapter.fetchKeysForId(mint.mintUrl, ks.id);
				const keypairs = Object.fromEntries(Object.entries(keysRes).map(([k, v]) => [Number(k), v]));
				return this.keysetRepo.addKeyset({
					mintUrl: mint.mintUrl,
					id: ks.id,
					unit: ks.unit,
					keypairs,
					active: ks.active,
					feePpk: ks.input_fee_ppk || 0
				});
			} catch (err) {
				this.logger?.error("Failed to sync keyset", {
					mintUrl: mint.mintUrl,
					keysetId: ks.id,
					err
				});
				throw new KeysetSyncError(mint.mintUrl, ks.id, void 0, err);
			}
		}));
		mint.mintInfo = mintInfo;
		mint.updatedAt = Math.floor(Date.now() / 1e3);
		await this.mintRepo.addOrUpdateMint(mint);
		const repoKeysets = await this.keysetRepo.getKeysetsByMintUrl(mint.mintUrl);
		this.logger?.info("Mint updated", {
			mintUrl: mint.mintUrl,
			keysets: repoKeysets.length
		});
		return {
			mint,
			keysets: repoKeysets
		};
	}
};

//#endregion
//#region infra/RequestRateLimiter.ts
/**
* Token-bucket based request rate limiter that exposes a request-compatible API
* for the cashu-ts `_customRequest` parameter.
*
* - Token capacity determines max burst size.
* - Tokens refill continuously based on `refillPerMinute`.
* - Paths starting with any configured prefix are not throttled.
* - Requests are queued FIFO when tokens are exhausted.
*/
var RequestRateLimiter = class {
	capacity;
	refillPerMinute;
	tokens;
	lastRefillAt;
	bypassPathPrefixes;
	logger;
	queue = [];
	processingTimer = null;
	constructor(options) {
		this.capacity = Math.max(1, options?.capacity ?? 25);
		this.refillPerMinute = Math.max(1, options?.refillPerMinute ?? 25);
		this.tokens = this.capacity;
		this.lastRefillAt = Date.now();
		this.bypassPathPrefixes = options?.bypassPathPrefixes ?? [];
		this.logger = options?.logger;
	}
	/**
	* The request function compatible with cashu-ts's `request(options)` signature.
	* It uses the global fetch under the hood.
	*/
	request = async (options) => {
		const url = new URL(options.endpoint);
		if (this.shouldBypass(url.pathname)) return this.performFetch(options);
		await this.acquireToken();
		try {
			return await this.performFetch(options);
		} finally {
			this.scheduleProcessingIfNeeded();
		}
	};
	shouldBypass(pathname) {
		if (!this.bypassPathPrefixes.length) return false;
		return this.bypassPathPrefixes.some((p) => pathname.startsWith(p));
	}
	performFetch = async (options) => {
		const { endpoint, requestBody, headers,...init } = options;
		const finalHeaders = new Headers({
			Accept: "application/json, text/plain, */*",
			...headers || {}
		});
		let body = void 0;
		if (requestBody !== void 0) {
			finalHeaders.set("Content-Type", "application/json");
			body = JSON.stringify(requestBody);
		}
		let response;
		try {
			response = await fetch(endpoint, {
				...init,
				headers: finalHeaders,
				body
			});
		} catch (err) {
			throw new NetworkError(err instanceof Error ? err.message : "Network request failed");
		}
		if (!response.ok) {
			let errorData = { error: "bad response" };
			try {
				errorData = await response.clone().json();
			} catch {}
			if (response.status === 400 && errorData && typeof errorData.code === "number" && typeof errorData.detail === "string") {
				const { code, detail } = errorData;
				throw new MintOperationError(code, detail);
			}
			let errorMessage = "HTTP request failed";
			const anyErr = errorData;
			if (typeof anyErr?.error === "string") errorMessage = anyErr.error;
			else if (typeof anyErr?.detail === "string") errorMessage = anyErr.detail;
			throw new HttpResponseError(errorMessage, response.status);
		}
		try {
			return await response.json();
		} catch (err) {
			this.logger?.error("Failed to parse HTTP response", err);
			throw new HttpResponseError("bad response", response.status);
		}
	};
	acquireToken() {
		this.refillTokens();
		if (this.tokens >= 1) {
			this.tokens -= 1;
			this.logger?.debug("RateLimiter token granted immediately", {
				tokens: this.tokens,
				capacity: this.capacity
			});
			return Promise.resolve();
		}
		return new Promise((resolve) => {
			this.queue.push(() => {
				resolve();
			});
			this.logger?.debug("Queued request due to empty bucket", { queueLength: this.queue.length });
			this.scheduleProcessingIfNeeded();
		});
	}
	scheduleProcessingIfNeeded() {
		if (this.processingTimer) return;
		const delayMs = this.msUntilNextToken();
		this.processingTimer = setTimeout(() => {
			this.processingTimer = null;
			this.processQueue();
		}, delayMs);
	}
	processQueue() {
		this.refillTokens();
		while (this.tokens >= 1 && this.queue.length > 0) {
			const next = this.queue.shift();
			if (!next) continue;
			this.tokens -= 1;
			try {
				next();
			} catch (err) {
				this.logger?.error("RateLimiter queue task error", err);
			}
		}
		if (this.queue.length > 0) this.scheduleProcessingIfNeeded();
	}
	refillTokens() {
		const now = Date.now();
		const elapsedMs = now - this.lastRefillAt;
		if (elapsedMs <= 0) return;
		const tokensPerMs = this.refillPerMinute / 6e4;
		const refill = elapsedMs * tokensPerMs;
		const newTokens = Math.min(this.capacity, this.tokens + refill);
		if (newTokens !== this.tokens) {
			this.tokens = newTokens;
			this.lastRefillAt = now;
		} else this.lastRefillAt = now;
	}
	msUntilNextToken() {
		this.refillTokens();
		if (this.tokens >= 1) return 0;
		const tokensPerMs = this.refillPerMinute / 6e4;
		const deficit = 1 - this.tokens;
		return Math.max(1, Math.ceil(deficit / tokensPerMs));
	}
};

//#endregion
//#region services/WalletService.ts
const DEFAULT_UNIT = "sat";
var WalletService = class {
	walletCache = /* @__PURE__ */ new Map();
	CACHE_TTL = 300 * 1e3;
	mintService;
	seedService;
	inFlight = /* @__PURE__ */ new Map();
	logger;
	requestLimiters = /* @__PURE__ */ new Map();
	requestLimiterOptionsForMint;
	constructor(mintService, seedService, logger, requestLimiterOptionsForMint) {
		this.mintService = mintService;
		this.seedService = seedService;
		this.logger = logger;
		this.requestLimiterOptionsForMint = requestLimiterOptionsForMint;
	}
	async getWallet(mintUrl) {
		if (!mintUrl || mintUrl.trim().length === 0) throw new Error("mintUrl is required");
		const cached = this.walletCache.get(mintUrl);
		if (cached && Date.now() - cached.lastCheck < this.CACHE_TTL) {
			this.logger?.debug("Wallet served from cache", { mintUrl });
			return cached.wallet;
		}
		const existing = this.inFlight.get(mintUrl);
		if (existing) return existing;
		const promise = this.buildWallet(mintUrl).finally(() => {
			this.inFlight.delete(mintUrl);
		});
		this.inFlight.set(mintUrl, promise);
		return promise;
	}
	async getWalletWithActiveKeysetId(mintUrl) {
		const wallet = await this.getWallet(mintUrl);
		const keyset = wallet.getActiveKeyset(wallet.keysets);
		const keys = await wallet.getKeys(keyset.id);
		return {
			wallet,
			keysetId: keyset.id,
			keyset,
			keys
		};
	}
	/**
	* Clear cached wallet for a specific mint URL
	*/
	clearCache(mintUrl) {
		this.walletCache.delete(mintUrl);
		this.logger?.debug("Wallet cache cleared", { mintUrl });
	}
	/**
	* Clear all cached wallets
	*/
	clearAllCaches() {
		this.walletCache.clear();
		this.logger?.debug("All wallet caches cleared");
	}
	/**
	* Force refresh mint data and get fresh wallet
	*/
	async refreshWallet(mintUrl) {
		this.clearCache(mintUrl);
		this.inFlight.delete(mintUrl);
		await this.mintService.updateMintData(mintUrl);
		return this.getWallet(mintUrl);
	}
	async buildWallet(mintUrl) {
		const { mint, keysets } = await this.mintService.ensureUpdatedMint(mintUrl);
		const validKeysets = keysets.filter((keyset) => keyset.keypairs && Object.keys(keyset.keypairs).length > 0 && keyset.unit === DEFAULT_UNIT);
		if (validKeysets.length === 0) throw new Error(`No valid keysets found for mint ${mintUrl}`);
		const keys = validKeysets.map((keyset) => ({
			id: keyset.id,
			unit: keyset.unit,
			keys: keyset.keypairs
		}));
		const compatibleKeysets = validKeysets.map((k) => ({
			id: k.id,
			unit: "sat",
			active: k.active,
			input_fee_ppk: k.feePpk
		}));
		const seed = await this.seedService.getSeed();
		const requestLimiter = this.getOrCreateRequestLimiter(mintUrl);
		const wallet = new __cashu_cashu_ts.CashuWallet(new __cashu_cashu_ts.CashuMint(mintUrl, requestLimiter.request), {
			mintInfo: mint.mintInfo,
			unit: DEFAULT_UNIT,
			keys,
			keysets: compatibleKeysets,
			logger: this.logger && this.logger.child ? this.logger.child({ module: "Wallet" }) : void 0,
			bip39seed: seed
		});
		this.walletCache.set(mintUrl, {
			wallet,
			lastCheck: Date.now()
		});
		this.logger?.info("Wallet built", {
			mintUrl,
			keysetCount: validKeysets.length
		});
		return wallet;
	}
	getOrCreateRequestLimiter(mintUrl) {
		const existing = this.requestLimiters.get(mintUrl);
		if (existing) return existing;
		const defaults = this.requestLimiterOptionsForMint?.(mintUrl) ?? {};
		const limiter = new RequestRateLimiter({
			capacity: 20,
			refillPerMinute: 20,
			bypassPathPrefixes: [],
			...defaults,
			logger: this.logger?.child ? this.logger.child({ module: "RequestRateLimiter" }) : this.logger
		});
		this.requestLimiters.set(mintUrl, limiter);
		return limiter;
	}
};

//#endregion
//#region services/ProofService.ts
var ProofService = class {
	counterService;
	proofRepository;
	eventBus;
	walletService;
	keyRingService;
	seedService;
	logger;
	constructor(counterService, proofRepository, walletService, keyRingService, seedService, logger, eventBus) {
		this.counterService = counterService;
		this.walletService = walletService;
		this.keyRingService = keyRingService;
		this.proofRepository = proofRepository;
		this.seedService = seedService;
		this.logger = logger;
		this.eventBus = eventBus;
	}
	/**
	* Calculates the send amount including receiver fees.
	* This is used when the sender pays fees for the receiver.
	*/
	async calculateSendAmountWithFees(mintUrl, sendAmount) {
		const { wallet, keys, keysetId } = await this.walletService.getWalletWithActiveKeysetId(mintUrl);
		let denominations = splitAmount(sendAmount, keys.keys);
		let receiveFee = wallet.getFeesForKeyset(denominations.length, keysetId);
		let receiveFeeAmounts = splitAmount(receiveFee, keys.keys);
		while (wallet.getFeesForKeyset(denominations.length + receiveFeeAmounts.length, keysetId) > receiveFee) {
			receiveFee++;
			receiveFeeAmounts = splitAmount(receiveFee, keys.keys);
		}
		return sendAmount + receiveFee;
	}
	async createOutputsAndIncrementCounters(mintUrl, amount, options) {
		if (!mintUrl || mintUrl.trim().length === 0) throw new ProofValidationError("mintUrl is required");
		if (!Number.isFinite(amount.keep) || !Number.isFinite(amount.send) || amount.keep < 0 || amount.send < 0) return {
			keep: [],
			send: [],
			sendAmount: 0,
			keepAmount: 0
		};
		const { wallet, keys, keysetId } = await this.walletService.getWalletWithActiveKeysetId(mintUrl);
		const seed = await this.seedService.getSeed();
		const currentCounter = await this.counterService.getCounter(mintUrl, keys.id);
		const data = {
			keep: [],
			send: []
		};
		let sendAmount = amount.send;
		let keepAmount = amount.keep;
		if (options?.includeFees && amount.send > 0) {
			sendAmount = await this.calculateSendAmountWithFees(mintUrl, amount.send);
			const feeAmount = sendAmount - amount.send;
			keepAmount = Math.max(0, amount.keep - feeAmount);
			this.logger?.debug("Fee calculation for send amount", {
				mintUrl,
				originalSendAmount: amount.send,
				originalKeepAmount: amount.keep,
				feeAmount,
				finalSendAmount: sendAmount,
				adjustedKeepAmount: keepAmount
			});
		}
		if (keepAmount > 0) {
			data.keep = __cashu_cashu_ts.OutputData.createDeterministicData(keepAmount, seed, currentCounter.counter, keys);
			if (data.keep.length > 0) await this.counterService.incrementCounter(mintUrl, keys.id, data.keep.length);
		}
		if (sendAmount > 0) {
			data.send = __cashu_cashu_ts.OutputData.createDeterministicData(sendAmount, seed, currentCounter.counter + data.keep.length, keys);
			if (data.send.length > 0) await this.counterService.incrementCounter(mintUrl, keys.id, data.send.length);
		}
		this.logger?.debug("Deterministic outputs created", {
			mintUrl,
			keysetId: keys.id,
			amount,
			outputs: data.keep.length + data.send.length
		});
		return {
			keep: data.keep,
			send: data.send,
			sendAmount,
			keepAmount
		};
	}
	async saveProofs(mintUrl, proofs) {
		if (!mintUrl || mintUrl.trim().length === 0) throw new ProofValidationError("mintUrl is required");
		if (!Array.isArray(proofs) || proofs.length === 0) return;
		const groupedByKeyset = this.groupProofsByKeysetId(proofs);
		const tasks = Array.from(groupedByKeyset.entries()).map(([keysetId, group]) => (async () => {
			await this.proofRepository.saveProofs(mintUrl, group);
			await this.eventBus?.emit("proofs:saved", {
				mintUrl,
				keysetId,
				proofs: group
			});
			this.logger?.info("Proofs saved", {
				mintUrl,
				keysetId,
				count: group.length
			});
		})().catch((error) => {
			throw {
				keysetId,
				error
			};
		}));
		const failed = (await Promise.allSettled(tasks)).filter((r) => r.status === "rejected");
		if (failed.length > 0) {
			for (const fr of failed) {
				const { keysetId, error } = fr.reason;
				this.logger?.error("Failed to persist proofs for keyset", {
					mintUrl,
					keysetId,
					error
				});
			}
			const details = failed.map((fr) => fr.reason);
			const failedKeysets = details.map((d) => d.keysetId).filter((id) => Boolean(id));
			const aggregate = new AggregateError(details.map((d) => d?.error instanceof Error ? d.error : new Error(String(d?.error))), `Failed to persist proofs for ${failed.length} keyset group(s)`);
			const message = failedKeysets.length > 0 ? `Failed to persist proofs for ${failed.length} keyset group(s) [${failedKeysets.join(", ")}]` : `Failed to persist proofs for ${failed.length} keyset group(s)`;
			throw new ProofOperationError(mintUrl, message, void 0, aggregate);
		}
	}
	async getReadyProofs(mintUrl) {
		return this.proofRepository.getReadyProofs(mintUrl);
	}
	async getAllReadyProofs() {
		return this.proofRepository.getAllReadyProofs();
	}
	/**
	* Gets the balance for a single mint by summing ready proof amounts.
	* @param mintUrl - The URL of the mint
	* @returns The total balance for the mint
	*/
	async getBalance(mintUrl) {
		if (!mintUrl || mintUrl.trim().length === 0) throw new ProofValidationError("mintUrl is required");
		return (await this.getReadyProofs(mintUrl)).reduce((acc, proof) => acc + proof.amount, 0);
	}
	/**
	* Gets balances for all mints by summing ready proof amounts.
	* @returns An object mapping mint URLs to their balances
	*/
	async getBalances() {
		const proofs = await this.getAllReadyProofs();
		const balances = {};
		for (const proof of proofs) {
			const mintUrl = proof.mintUrl;
			balances[mintUrl] = (balances[mintUrl] || 0) + proof.amount;
		}
		return balances;
	}
	async setProofState(mintUrl, secrets, state) {
		if (!mintUrl || mintUrl.trim().length === 0) throw new ProofValidationError("mintUrl is required");
		if (!secrets || secrets.length === 0) return;
		await this.proofRepository.setProofState(mintUrl, secrets, state);
		await this.eventBus?.emit("proofs:state-changed", {
			mintUrl,
			secrets,
			state
		});
		this.logger?.debug("Proof state updated", {
			mintUrl,
			count: secrets.length,
			state
		});
	}
	async deleteProofs(mintUrl, secrets) {
		if (!mintUrl || mintUrl.trim().length === 0) throw new ProofValidationError("mintUrl is required");
		if (!secrets || secrets.length === 0) return;
		await this.proofRepository.deleteProofs(mintUrl, secrets);
		await this.eventBus?.emit("proofs:deleted", {
			mintUrl,
			secrets
		});
		this.logger?.info("Proofs deleted", {
			mintUrl,
			count: secrets.length
		});
	}
	async wipeProofsByKeysetId(mintUrl, keysetId) {
		if (!mintUrl || mintUrl.trim().length === 0) throw new ProofValidationError("mintUrl is required");
		if (!keysetId || keysetId.trim().length === 0) throw new ProofValidationError("keysetId is required");
		await this.proofRepository.wipeProofsByKeysetId(mintUrl, keysetId);
		await this.eventBus?.emit("proofs:wiped", {
			mintUrl,
			keysetId
		});
		this.logger?.info("Proofs wiped by keyset", {
			mintUrl,
			keysetId
		});
	}
	async selectProofsToSend(mintUrl, amount, includeFees = true) {
		const proofs = await this.getReadyProofs(mintUrl);
		if (proofs.reduce((acc, proof) => acc + proof.amount, 0) < amount) throw new ProofValidationError("Not enough proofs to send");
		const selectedProofs = (await this.walletService.getWallet(mintUrl)).selectProofsToSend(proofs, amount, includeFees);
		this.logger?.debug("Selected proofs to send", {
			mintUrl,
			amount,
			selectedProofs,
			count: selectedProofs.send.length
		});
		return selectedProofs.send;
	}
	groupProofsByKeysetId(proofs) {
		const map = /* @__PURE__ */ new Map();
		for (const proof of proofs) {
			if (!proof.secret) throw new ProofValidationError("Proof missing secret");
			const keysetId = proof.id;
			if (!keysetId || keysetId.trim().length === 0) throw new ProofValidationError("Proof missing keyset id");
			const existing = map.get(keysetId);
			if (existing) existing.push(proof);
			else map.set(keysetId, [proof]);
		}
		return map;
	}
	async getProofsByKeysetId(mintUrl, keysetId) {
		return this.proofRepository.getProofsByKeysetId(mintUrl, keysetId);
	}
	async hasProofsForKeyset(mintUrl, keysetId) {
		if (!mintUrl || mintUrl.trim().length === 0) throw new ProofValidationError("mintUrl is required");
		if (!keysetId || keysetId.trim().length === 0) throw new ProofValidationError("keysetId is required");
		const proofs = await this.proofRepository.getProofsByKeysetId(mintUrl, keysetId);
		const hasProofs = proofs.length > 0;
		this.logger?.debug("Checked proofs for keyset", {
			mintUrl,
			keysetId,
			hasProofs,
			totalProofs: proofs.length
		});
		return hasProofs;
	}
	async prepareProofsForReceiving(proofs) {
		this.logger?.debug("Preparing proofs for receiving", { totalProofs: proofs.length });
		const preparedProofs = [...proofs];
		let regularProofCount = 0;
		let p2pkProofCount = 0;
		for (let i = 0; i < preparedProofs.length; i++) {
			const proof = preparedProofs[i];
			if (!proof) continue;
			let parsedSecret;
			try {
				parsedSecret = JSON.parse(proof.secret);
			} catch (parseError) {
				this.logger?.debug("Regular proof detected, skipping P2PK processing", { proofIndex: i });
				regularProofCount++;
				continue;
			}
			if (parsedSecret[0] !== "P2PK") {
				this.logger?.error("Unsupported locking script type", {
					proofIndex: i,
					scriptType: parsedSecret[0]
				});
				throw new ProofValidationError("Only P2PK locking scripts are supported");
			}
			const additionalKeysTag = parsedSecret[1].tags.find((tag) => tag[0] === "pubkeys");
			if (additionalKeysTag && additionalKeysTag[1] && additionalKeysTag[1].length > 0) {
				this.logger?.error("Multisig P2PK proof detected", { proofIndex: i });
				throw new ProofValidationError("Multisig is not supported");
			}
			try {
				preparedProofs[i] = await this.keyRingService.signProof(proof, parsedSecret[1].data);
				this.logger?.debug("P2PK proof signed successfully", {
					proofIndex: i,
					recipient: parsedSecret[1].data
				});
				p2pkProofCount++;
			} catch (error) {
				this.logger?.error("Failed to sign P2PK proof for receiving", {
					proofIndex: i,
					recipient: parsedSecret[1].data,
					error
				});
				throw error;
			}
		}
		this.logger?.info("Proofs prepared for receiving", {
			totalProofs: proofs.length,
			regularProofs: regularProofCount,
			p2pkProofs: p2pkProofCount
		});
		return preparedProofs;
	}
};
/**
* Splits the amount into denominations of the provided keyset.
*
* @remarks
* Partial splits will be filled up to value using minimum splits required. Sorting is only applied
* if a fill was made - exact custom splits are always returned in the same order.
* @param value Amount to split.
* @param keyset Keys to look up split amounts.
* @param split? Optional custom split amounts.
* @param order? Optional order for split amounts (if fill was required)
* @returns Array of split amounts.
* @throws Error if split sum is greater than value or mint does not have keys for requested split.
*/
function splitAmount(value, keys) {
	const split = [];
	const sortedKeyAmounts = Object.keys(keys).map((key) => Number(key)).sort((a, b) => b - a);
	if (!sortedKeyAmounts || sortedKeyAmounts.length === 0) throw new Error("Cannot split amount, keyset is inactive or contains no keys");
	for (const amt of sortedKeyAmounts) {
		if (amt <= 0) continue;
		const requireCount = Math.floor(value / amt);
		split.push(...Array(requireCount).fill(amt));
		value -= amt * requireCount;
		if (value === 0) break;
	}
	if (value !== 0) throw new Error(`Unable to split remaining amount: ${value}`);
	return split;
}

//#endregion
//#region services/MintQuoteService.ts
var MintQuoteService = class {
	mintQuoteRepo;
	mintService;
	walletService;
	proofService;
	eventBus;
	logger;
	constructor(mintQuoteRepo, mintService, walletService, proofService, eventBus, logger) {
		this.mintQuoteRepo = mintQuoteRepo;
		this.mintService = mintService;
		this.walletService = walletService;
		this.proofService = proofService;
		this.eventBus = eventBus;
		this.logger = logger;
	}
	async createMintQuote(mintUrl, amount) {
		this.logger?.info("Creating mint quote", {
			mintUrl,
			amount
		});
		if (!await this.mintService.isTrustedMint(mintUrl)) throw new UnknownMintError(`Mint ${mintUrl} is not trusted`);
		try {
			const { wallet } = await this.walletService.getWalletWithActiveKeysetId(mintUrl);
			const quote = await wallet.createMintQuote(amount);
			await this.mintQuoteRepo.addMintQuote({
				...quote,
				mintUrl
			});
			await this.eventBus.emit("mint-quote:created", {
				mintUrl,
				quoteId: quote.quote,
				quote
			});
			return quote;
		} catch (err) {
			this.logger?.error("Failed to create mint quote", {
				mintUrl,
				amount,
				err
			});
			throw err;
		}
	}
	async redeemMintQuote(mintUrl, quoteId) {
		this.logger?.info("Redeeming mint quote", {
			mintUrl,
			quoteId
		});
		if (!await this.mintService.isTrustedMint(mintUrl)) throw new UnknownMintError(`Mint ${mintUrl} is not trusted`);
		try {
			const quote = await this.mintQuoteRepo.getMintQuote(mintUrl, quoteId);
			if (!quote) {
				this.logger?.warn("Mint quote not found", {
					mintUrl,
					quoteId
				});
				throw new Error("Quote not found");
			}
			const { wallet } = await this.walletService.getWalletWithActiveKeysetId(mintUrl);
			const { keep } = await this.proofService.createOutputsAndIncrementCounters(mintUrl, {
				keep: quote.amount,
				send: 0
			});
			const proofs = await wallet.mintProofs(quote.amount, quote.quote, { outputData: keep });
			await this.eventBus.emit("mint-quote:redeemed", {
				mintUrl,
				quoteId,
				quote
			});
			this.logger?.info("Mint quote redeemed, proofs minted", {
				mintUrl,
				quoteId,
				amount: quote.amount,
				proofs: proofs.length
			});
			await this.setMintQuoteState(mintUrl, quoteId, "ISSUED");
			await this.proofService.saveProofs(mintUrl, mapProofToCoreProof(mintUrl, "ready", proofs));
			this.logger?.debug("Proofs saved to repository", {
				mintUrl,
				count: proofs.length
			});
		} catch (err) {
			this.logger?.error("Failed to redeem mint quote", {
				mintUrl,
				quoteId,
				err
			});
			throw err;
		}
	}
	async addExistingMintQuotes(mintUrl, quotes) {
		this.logger?.info("Adding existing mint quotes", {
			mintUrl,
			count: quotes.length
		});
		const added = [];
		const skipped = [];
		for (const quote of quotes) try {
			if (await this.mintQuoteRepo.getMintQuote(mintUrl, quote.quote)) {
				this.logger?.debug("Quote already exists, skipping", {
					mintUrl,
					quoteId: quote.quote
				});
				skipped.push(quote.quote);
				continue;
			}
			await this.mintQuoteRepo.addMintQuote({
				...quote,
				mintUrl
			});
			added.push(quote.quote);
			await this.eventBus.emit("mint-quote:added", {
				mintUrl,
				quoteId: quote.quote,
				quote
			});
			this.logger?.debug("Added existing mint quote", {
				mintUrl,
				quoteId: quote.quote,
				state: quote.state
			});
		} catch (err) {
			this.logger?.error("Failed to add existing mint quote", {
				mintUrl,
				quoteId: quote.quote,
				err
			});
			skipped.push(quote.quote);
		}
		this.logger?.info("Finished adding existing mint quotes", {
			mintUrl,
			added: added.length,
			skipped: skipped.length
		});
		return {
			added,
			skipped
		};
	}
	async updateStateFromRemote(mintUrl, quoteId, state) {
		this.logger?.info("Updating mint quote state from remote", {
			mintUrl,
			quoteId,
			state
		});
		await this.setMintQuoteState(mintUrl, quoteId, state);
	}
	async setMintQuoteState(mintUrl, quoteId, state) {
		this.logger?.debug("Setting mint quote state", {
			mintUrl,
			quoteId,
			state
		});
		await this.mintQuoteRepo.setMintQuoteState(mintUrl, quoteId, state);
		await this.eventBus.emit("mint-quote:state-changed", {
			mintUrl,
			quoteId,
			state
		});
		this.logger?.debug("Mint quote state updated", {
			mintUrl,
			quoteId,
			state
		});
	}
	/**
	* Requeue all PAID (but not yet ISSUED) quotes for processing.
	* Only requeues quotes for trusted mints.
	* Emits `mint-quote:requeue` for each PAID quote so the processor can enqueue them.
	*/
	async requeuePaidMintQuotes(mintUrl) {
		const requeued = [];
		try {
			const pending = await this.mintQuoteRepo.getPendingMintQuotes();
			for (const q of pending) {
				if (mintUrl && q.mintUrl !== mintUrl) continue;
				if (q.state !== "PAID") continue;
				if (!await this.mintService.isTrustedMint(q.mintUrl)) {
					this.logger?.debug("Skipping requeue for untrusted mint", {
						mintUrl: q.mintUrl,
						quoteId: q.quote
					});
					continue;
				}
				await this.eventBus.emit("mint-quote:requeue", {
					mintUrl: q.mintUrl,
					quoteId: q.quote
				});
				requeued.push(q.quote);
			}
			this.logger?.info("Requeued PAID mint quotes", {
				count: requeued.length,
				mintUrl
			});
		} catch (err) {
			this.logger?.error("Failed to requeue PAID mint quotes", {
				mintUrl,
				err
			});
		}
		return { requeued };
	}
};

//#endregion
//#region services/watchers/MintQuoteWatcherService.ts
function toKey$1(mintUrl, quoteId) {
	return `${mintUrl}::${quoteId}`;
}
var MintQuoteWatcherService = class {
	repo;
	subs;
	mintService;
	quotes;
	bus;
	logger;
	options;
	running = false;
	unsubscribeByKey = /* @__PURE__ */ new Map();
	offCreated;
	offAdded;
	offUntrusted;
	constructor(repo, subs, mintService, quotes, bus, logger, options = { watchExistingPendingOnStart: true }) {
		this.repo = repo;
		this.subs = subs;
		this.mintService = mintService;
		this.quotes = quotes;
		this.bus = bus;
		this.logger = logger;
		this.options = options;
	}
	isRunning() {
		return this.running;
	}
	async start() {
		if (this.running) return;
		this.running = true;
		this.logger?.info("MintQuoteWatcherService started");
		this.offCreated = this.bus.on("mint-quote:created", async ({ mintUrl, quoteId }) => {
			try {
				await this.watchQuote(mintUrl, quoteId);
			} catch (err) {
				this.logger?.error("Failed to start watching quote from event", {
					mintUrl,
					quoteId,
					err
				});
			}
		});
		this.offAdded = this.bus.on("mint-quote:added", async ({ mintUrl, quoteId, quote }) => {
			if (quote.state !== "ISSUED" && quote.state !== "PAID") try {
				await this.watchQuote(mintUrl, quoteId);
			} catch (err) {
				this.logger?.error("Failed to start watching added quote", {
					mintUrl,
					quoteId,
					state: quote.state,
					err
				});
			}
		});
		this.offUntrusted = this.bus.on("mint:untrusted", async ({ mintUrl }) => {
			try {
				await this.stopWatchingMint(mintUrl);
			} catch (err) {
				this.logger?.error("Failed to stop watching mint quotes on untrust", {
					mintUrl,
					err
				});
			}
		});
		if (this.options.watchExistingPendingOnStart) try {
			const pending = await this.repo.getPendingMintQuotes();
			const byMint = /* @__PURE__ */ new Map();
			for (const q of pending) {
				let arr = byMint.get(q.mintUrl);
				if (!arr) {
					arr = [];
					byMint.set(q.mintUrl, arr);
				}
				arr.push(q.quote);
			}
			for (const [mintUrl, quoteIds] of byMint.entries()) {
				if (!await this.mintService.isTrustedMint(mintUrl)) {
					this.logger?.debug("Skipping pending quotes for untrusted mint", {
						mintUrl,
						count: quoteIds.length
					});
					continue;
				}
				try {
					await this.watchQuote(mintUrl, quoteIds);
				} catch (err) {
					this.logger?.warn("Failed to watch pending quotes batch", {
						mintUrl,
						count: quoteIds.length,
						err
					});
				}
			}
		} catch (err) {
			this.logger?.error("Failed to load pending mint quotes to watch", { err });
		}
	}
	async stop() {
		if (!this.running) return;
		this.running = false;
		if (this.offCreated) try {
			this.offCreated();
		} catch {} finally {
			this.offCreated = void 0;
		}
		if (this.offAdded) try {
			this.offAdded();
		} catch {} finally {
			this.offAdded = void 0;
		}
		if (this.offUntrusted) try {
			this.offUntrusted();
		} catch {} finally {
			this.offUntrusted = void 0;
		}
		const entries = Array.from(this.unsubscribeByKey.entries());
		this.unsubscribeByKey.clear();
		for (const [key, unsub] of entries) try {
			await unsub();
			this.logger?.debug("Stopped watching quote", { key });
		} catch (err) {
			this.logger?.warn("Failed to unsubscribe watcher", {
				key,
				err
			});
		}
		this.logger?.info("MintQuoteWatcherService stopped");
	}
	async watchQuote(mintUrl, quoteOrQuotes) {
		if (!this.running) return;
		if (!await this.mintService.isTrustedMint(mintUrl)) {
			this.logger?.debug("Skipping watch for untrusted mint", { mintUrl });
			return;
		}
		const input = Array.isArray(quoteOrQuotes) ? quoteOrQuotes : [quoteOrQuotes];
		const toWatch = Array.from(new Set(input)).filter((id) => !this.unsubscribeByKey.has(toKey$1(mintUrl, id)));
		if (toWatch.length === 0) return;
		const chunks = [];
		for (let i = 0; i < toWatch.length; i += 100) chunks.push(toWatch.slice(i, i + 100));
		for (const batch of chunks) {
			const { subId, unsubscribe } = await this.subs.subscribe(mintUrl, "bolt11_mint_quote", batch, async (payload) => {
				if (payload.state !== "PAID" && payload.state !== "ISSUED") return;
				const quoteId = payload.quote;
				if (!quoteId) return;
				const key = toKey$1(mintUrl, quoteId);
				try {
					await this.quotes.updateStateFromRemote(mintUrl, quoteId, payload.state);
					this.logger?.debug("Updated quote state from remote", {
						mintUrl,
						quoteId,
						state: payload.state,
						subId
					});
				} catch (err) {
					this.logger?.error("Failed to update quote state from remote", {
						mintUrl,
						quoteId,
						state: payload.state,
						err
					});
				}
				if (payload.state === "ISSUED") await this.stopWatching(key);
			});
			let didUnsubscribe = false;
			const remaining = new Set(batch);
			const groupUnsubscribeOnce = async () => {
				if (didUnsubscribe) return;
				didUnsubscribe = true;
				await unsubscribe();
				this.logger?.debug("Unsubscribed watcher for mint quote batch", {
					mintUrl,
					subId
				});
			};
			for (const quoteId of batch) {
				const key = toKey$1(mintUrl, quoteId);
				const perKeyStop = async () => {
					if (remaining.has(quoteId)) remaining.delete(quoteId);
					if (remaining.size === 0) await groupUnsubscribeOnce();
				};
				this.unsubscribeByKey.set(key, perKeyStop);
			}
			this.logger?.debug("Watching mint quote batch", {
				mintUrl,
				subId,
				filterCount: batch.length
			});
		}
	}
	async stopWatching(key) {
		const unsubscribe = this.unsubscribeByKey.get(key);
		if (!unsubscribe) return;
		try {
			await unsubscribe();
		} catch (err) {
			this.logger?.warn("Unsubscribe watcher failed", {
				key,
				err
			});
		} finally {
			this.unsubscribeByKey.delete(key);
		}
	}
	async stopWatchingMint(mintUrl) {
		this.logger?.info("Stopping all quote watchers for mint", { mintUrl });
		const prefix = `${mintUrl}::`;
		const keysToStop = [];
		for (const key of this.unsubscribeByKey.keys()) if (key.startsWith(prefix)) keysToStop.push(key);
		for (const key of keysToStop) await this.stopWatching(key);
		this.logger?.info("Stopped quote watchers for mint", {
			mintUrl,
			count: keysToStop.length
		});
	}
};

//#endregion
//#region services/watchers/MintQuoteProcessor.ts
var Bolt11QuoteHandler = class {
	constructor(quotes, logger) {
		this.quotes = quotes;
		this.logger = logger;
	}
	canHandle(quoteType) {
		return quoteType === "bolt11";
	}
	async process(mintUrl, quoteId) {
		await this.quotes.redeemMintQuote(mintUrl, quoteId);
	}
};
var MintQuoteProcessor = class {
	quotes;
	bus;
	logger;
	running = false;
	queue = [];
	processing = false;
	processingTimer;
	offStateChanged;
	offQuoteAdded;
	offRequeue;
	offUntrusted;
	handlers = /* @__PURE__ */ new Map();
	processIntervalMs;
	maxRetries;
	baseRetryDelayMs;
	initialEnqueueDelayMs;
	constructor(quotes, bus, logger, options) {
		this.quotes = quotes;
		this.bus = bus;
		this.logger = logger;
		this.processIntervalMs = options?.processIntervalMs ?? 3e3;
		this.maxRetries = options?.maxRetries ?? 3;
		this.baseRetryDelayMs = options?.baseRetryDelayMs ?? 5e3;
		this.initialEnqueueDelayMs = options?.initialEnqueueDelayMs ?? 500;
		this.registerHandler("bolt11", new Bolt11QuoteHandler(quotes, logger));
	}
	registerHandler(quoteType, handler) {
		this.handlers.set(quoteType, handler);
		this.logger?.debug("Registered quote handler", { quoteType });
	}
	isRunning() {
		return this.running;
	}
	async start() {
		if (this.running) return;
		this.running = true;
		this.logger?.info("MintQuoteProcessor started");
		this.offStateChanged = this.bus.on("mint-quote:state-changed", async ({ mintUrl, quoteId, state }) => {
			if (state === "PAID") this.enqueue(mintUrl, quoteId, "bolt11");
		});
		this.offQuoteAdded = this.bus.on("mint-quote:added", async ({ mintUrl, quoteId, quote }) => {
			if (quote.state === "PAID") this.enqueue(mintUrl, quoteId, "bolt11");
		});
		this.offRequeue = this.bus.on("mint-quote:requeue", async ({ mintUrl, quoteId }) => {
			this.enqueue(mintUrl, quoteId, "bolt11");
		});
		this.offUntrusted = this.bus.on("mint:untrusted", ({ mintUrl }) => {
			this.clearMintFromQueue(mintUrl);
		});
		this.scheduleNextProcess();
	}
	async stop() {
		if (!this.running) return;
		this.running = false;
		if (this.offStateChanged) try {
			this.offStateChanged();
		} catch {} finally {
			this.offStateChanged = void 0;
		}
		if (this.offQuoteAdded) try {
			this.offQuoteAdded();
		} catch {} finally {
			this.offQuoteAdded = void 0;
		}
		if (this.offRequeue) try {
			this.offRequeue();
		} catch {} finally {
			this.offRequeue = void 0;
		}
		if (this.offUntrusted) try {
			this.offUntrusted();
		} catch {} finally {
			this.offUntrusted = void 0;
		}
		if (this.processingTimer) {
			clearTimeout(this.processingTimer);
			this.processingTimer = void 0;
		}
		while (this.processing) await new Promise((resolve) => setTimeout(resolve, 100));
		this.logger?.info("MintQuoteProcessor stopped", { pendingItems: this.queue.length });
	}
	/**
	* Wait for the queue to be empty and all processing to complete.
	* Useful for CLI applications that want to ensure all quotes are processed before exiting.
	*/
	async waitForCompletion() {
		while (this.queue.length > 0 || this.processing) await new Promise((resolve) => setTimeout(resolve, 100));
	}
	/**
	* Remove all queued items for a specific mint.
	* Called when a mint is untrusted to stop processing its quotes.
	*/
	clearMintFromQueue(mintUrl) {
		const before = this.queue.length;
		this.queue = this.queue.filter((item) => item.mintUrl !== mintUrl);
		const removed = before - this.queue.length;
		if (removed > 0) this.logger?.info("Cleared mint quotes from processor queue", {
			mintUrl,
			removed
		});
	}
	enqueue(mintUrl, quoteId, quoteType) {
		if (this.queue.find((item) => item.mintUrl === mintUrl && item.quoteId === quoteId)) {
			this.logger?.debug("Quote already in queue", {
				mintUrl,
				quoteId
			});
			return;
		}
		const wasEmpty = this.queue.length === 0;
		this.queue.push({
			mintUrl,
			quoteId,
			quoteType,
			retryCount: 0,
			nextRetryAt: 0
		});
		this.logger?.debug("Quote enqueued for processing", {
			mintUrl,
			quoteId,
			quoteType,
			queueLength: this.queue.length
		});
		if (wasEmpty && this.running && !this.processing) {
			if (this.processingTimer) {
				clearTimeout(this.processingTimer);
				this.processingTimer = void 0;
			}
			this.processingTimer = setTimeout(() => {
				this.processingTimer = void 0;
				this.processNext();
			}, this.initialEnqueueDelayMs);
		}
	}
	scheduleNextProcess() {
		if (!this.running || this.processingTimer) return;
		this.processingTimer = setTimeout(() => {
			this.processingTimer = void 0;
			this.processNext();
		}, this.processIntervalMs);
	}
	async processNext() {
		if (!this.running || this.processing || this.queue.length === 0) {
			if (this.running) this.scheduleNextProcess();
			return;
		}
		const now = Date.now();
		const readyIndex = this.queue.findIndex((item$1) => item$1.nextRetryAt <= now);
		if (readyIndex === -1) {
			const nextReady = Math.min(...this.queue.map((item$1) => item$1.nextRetryAt));
			const delay = Math.max(this.processIntervalMs, nextReady - now);
			this.processingTimer = setTimeout(() => {
				this.processingTimer = void 0;
				this.processNext();
			}, delay);
			return;
		}
		const [item] = this.queue.splice(readyIndex, 1);
		if (!item) return;
		this.processing = true;
		try {
			await this.processItem(item);
		} catch (err) {
			this.handleProcessingError(item, err);
		} finally {
			this.processing = false;
			if (this.running) this.scheduleNextProcess();
		}
	}
	async processItem(item) {
		const { mintUrl, quoteId, quoteType } = item;
		const handler = this.handlers.get(quoteType);
		if (!handler) {
			this.logger?.warn("No handler registered for quote type", {
				quoteType,
				mintUrl,
				quoteId
			});
			return;
		}
		this.logger?.info("Processing mint quote", {
			mintUrl,
			quoteId,
			quoteType,
			attempt: item.retryCount + 1
		});
		try {
			await handler.process(mintUrl, quoteId);
			this.logger?.info("Successfully processed mint quote", {
				mintUrl,
				quoteId,
				quoteType
			});
		} catch (err) {
			throw err;
		}
	}
	handleProcessingError(item, err) {
		const { mintUrl, quoteId } = item;
		if (err instanceof MintOperationError) {
			if (err.code === 20007) {
				this.logger?.warn("Mint quote expired", {
					mintUrl,
					quoteId
				});
				return;
			} else if (err.code === 20002) {
				this.logger?.info("Mint quote already issued, updating state", {
					mintUrl,
					quoteId
				});
				this.updateQuoteState(mintUrl, quoteId, "ISSUED");
				return;
			}
			this.logger?.error("Mint operation error, not retrying", {
				mintUrl,
				quoteId,
				code: err.code,
				detail: err.message
			});
			return;
		}
		if (err instanceof NetworkError || err instanceof Error && err.message.includes("network")) {
			item.retryCount++;
			if (item.retryCount <= this.maxRetries) {
				const delay = this.baseRetryDelayMs * Math.pow(2, item.retryCount - 1);
				item.nextRetryAt = Date.now() + delay;
				this.logger?.warn("Network error, will retry", {
					mintUrl,
					quoteId,
					attempt: item.retryCount,
					maxRetries: this.maxRetries,
					retryInMs: delay
				});
				this.queue.push(item);
				return;
			}
			this.logger?.error("Max retries exceeded for network error", {
				mintUrl,
				quoteId,
				maxRetries: this.maxRetries
			});
			return;
		}
		this.logger?.error("Failed to process mint quote", {
			mintUrl,
			quoteId,
			err
		});
	}
	async updateQuoteState(mintUrl, quoteId, state) {
		try {
			await this.quotes.updateStateFromRemote(mintUrl, quoteId, state);
		} catch (err) {
			this.logger?.error("Failed to update quote state", {
				mintUrl,
				quoteId,
				state,
				err
			});
		}
	}
};

//#endregion
//#region services/watchers/ProofStateWatcherService.ts
function toKey(mintUrl, secret) {
	return `${mintUrl}::${secret}`;
}
var ProofStateWatcherService = class {
	subs;
	mintService;
	proofs;
	bus;
	logger;
	options;
	running = false;
	unsubscribeByKey = /* @__PURE__ */ new Map();
	inflightByKey = /* @__PURE__ */ new Set();
	offProofsStateChanged;
	offUntrusted;
	constructor(subs, mintService, proofs, bus, logger, options = { watchExistingInflightOnStart: false }) {
		this.subs = subs;
		this.mintService = mintService;
		this.proofs = proofs;
		this.bus = bus;
		this.logger = logger;
		this.options = options;
	}
	isRunning() {
		return this.running;
	}
	async start() {
		if (this.running) return;
		this.running = true;
		this.logger?.info("ProofStateWatcherService started");
		this.offProofsStateChanged = this.bus.on("proofs:state-changed", async ({ mintUrl, secrets, state }) => {
			try {
				if (!this.running) return;
				if (state === "inflight") try {
					await this.watchProof(mintUrl, secrets);
				} catch (err) {
					this.logger?.warn("Failed to watch inflight proofs", {
						mintUrl,
						count: secrets.length,
						err
					});
				}
				else if (state === "spent") for (const secret of secrets) {
					const key = toKey(mintUrl, secret);
					try {
						await this.stopWatching(key);
					} catch (err) {
						this.logger?.warn("Failed to stop watcher on spent proof", {
							mintUrl,
							secret,
							err
						});
					}
				}
			} catch (err) {
				this.logger?.error("Error handling proofs:state-changed", { err });
			}
		});
		this.offUntrusted = this.bus.on("mint:untrusted", async ({ mintUrl }) => {
			try {
				await this.stopWatchingMint(mintUrl);
			} catch (err) {
				this.logger?.error("Failed to stop watching mint proofs on untrust", {
					mintUrl,
					err
				});
			}
		});
	}
	async stop() {
		if (!this.running) return;
		this.running = false;
		if (this.offProofsStateChanged) try {
			this.offProofsStateChanged();
		} catch {} finally {
			this.offProofsStateChanged = void 0;
		}
		if (this.offUntrusted) try {
			this.offUntrusted();
		} catch {} finally {
			this.offUntrusted = void 0;
		}
		const entries = Array.from(this.unsubscribeByKey.entries());
		this.unsubscribeByKey.clear();
		for (const [key, unsub] of entries) try {
			await unsub();
			this.logger?.debug("Stopped watching proof", { key });
		} catch (err) {
			this.logger?.warn("Failed to unsubscribe proof watcher", {
				key,
				err
			});
		}
		this.inflightByKey.clear();
		this.logger?.info("ProofStateWatcherService stopped");
	}
	async watchProof(mintUrl, secrets) {
		if (!this.running) return;
		if (!await this.mintService.isTrustedMint(mintUrl)) {
			this.logger?.debug("Skipping watch for untrusted mint", { mintUrl });
			return;
		}
		const toWatch = Array.from(new Set(secrets)).filter((secret) => !this.unsubscribeByKey.has(toKey(mintUrl, secret)));
		if (toWatch.length === 0) return;
		const { secretByYHex, yHexBySecret } = buildYHexMapsForSecrets(toWatch);
		const filters = Array.from(secretByYHex.keys());
		const { subId, unsubscribe } = await this.subs.subscribe(mintUrl, "proof_state", filters, async (payload) => {
			if (payload.state !== "SPENT") return;
			const secret = secretByYHex.get(payload.Y);
			if (!secret) return;
			const key = toKey(mintUrl, secret);
			if (this.inflightByKey.has(key)) return;
			this.inflightByKey.add(key);
			try {
				await this.proofs.setProofState(mintUrl, [secret], "spent");
				this.logger?.info("Marked inflight proof as spent from mint notification", {
					mintUrl,
					subId
				});
				await this.stopWatching(key);
			} catch (err) {
				this.logger?.error("Failed to mark inflight proof as spent", {
					mintUrl,
					subId,
					err
				});
			} finally {
				this.inflightByKey.delete(key);
			}
		});
		let didUnsubscribe = false;
		const remaining = new Set(filters);
		const groupUnsubscribeOnce = async () => {
			if (didUnsubscribe) return;
			didUnsubscribe = true;
			await unsubscribe();
			this.logger?.debug("Unsubscribed watcher for inflight proof group", {
				mintUrl,
				subId
			});
		};
		for (const secret of toWatch) {
			const key = toKey(mintUrl, secret);
			const yHex = yHexBySecret.get(secret);
			const perKeyStop = async () => {
				if (remaining.has(yHex)) remaining.delete(yHex);
				if (remaining.size === 0) await groupUnsubscribeOnce();
			};
			this.unsubscribeByKey.set(key, perKeyStop);
		}
		this.logger?.debug("Watching inflight proof states", {
			mintUrl,
			subId,
			filterCount: filters.length
		});
	}
	async stopWatching(key) {
		const unsubscribe = this.unsubscribeByKey.get(key);
		if (!unsubscribe) return;
		try {
			await unsubscribe();
		} catch (err) {
			this.logger?.warn("Unsubscribe proof watcher failed", {
				key,
				err
			});
		} finally {
			this.unsubscribeByKey.delete(key);
		}
	}
	async stopWatchingMint(mintUrl) {
		this.logger?.info("Stopping all proof watchers for mint", { mintUrl });
		const prefix = `${mintUrl}::`;
		const keysToStop = [];
		for (const key of this.unsubscribeByKey.keys()) if (key.startsWith(prefix)) keysToStop.push(key);
		for (const key of this.inflightByKey) if (key.startsWith(prefix)) this.inflightByKey.delete(key);
		for (const key of keysToStop) await this.stopWatching(key);
		this.logger?.info("Stopped proof watchers for mint", {
			mintUrl,
			count: keysToStop.length
		});
	}
};

//#endregion
//#region services/SeedService.ts
var SeedService = class {
	seedGetter;
	seedTtlMs;
	cachedSeed = null;
	cachedUntil = 0;
	inFlight = null;
	constructor(seedGetter, options) {
		this.seedGetter = seedGetter;
		this.seedTtlMs = Math.max(0, options?.seedTtlMs ?? 0);
	}
	async getSeed() {
		const now = Date.now();
		if (this.cachedSeed && now < this.cachedUntil) return new Uint8Array(this.cachedSeed);
		if (this.inFlight) {
			const seed = await this.inFlight;
			return new Uint8Array(seed);
		}
		this.inFlight = (async () => {
			const seed = await this.seedGetter();
			if (!(seed instanceof Uint8Array) || seed.length !== 64) throw new Error("SeedService: seedGetter must return a 64-byte Uint8Array");
			if (this.seedTtlMs > 0) {
				this.cachedSeed = new Uint8Array(seed);
				this.cachedUntil = Date.now() + this.seedTtlMs;
			} else {
				this.cachedSeed = null;
				this.cachedUntil = 0;
			}
			return seed;
		})();
		try {
			const seed = await this.inFlight;
			return new Uint8Array(seed);
		} finally {
			this.inFlight = null;
		}
	}
	clear() {
		this.cachedSeed = null;
		this.cachedUntil = 0;
	}
};

//#endregion
//#region services/WalletRestoreService.ts
var WalletRestoreService = class {
	proofService;
	counterService;
	walletService;
	logger;
	restoreBatchSize = 300;
	restoreGapLimit = 100;
	restoreStartCounter = 0;
	constructor(proofService, counterService, walletService, logger) {
		this.proofService = proofService;
		this.counterService = counterService;
		this.walletService = walletService;
		this.logger = logger;
	}
	async sweepKeyset(mintUrl, keysetId, bip39seed) {
		this.logger?.debug("Sweeping keyset", {
			mintUrl,
			keysetId
		});
		const { wallet } = await this.walletService.getWalletWithActiveKeysetId(mintUrl);
		const sweepWallet = new __cashu_cashu_ts.CashuWallet(new __cashu_cashu_ts.CashuMint(mintUrl), { bip39seed });
		const { proofs } = await sweepWallet.batchRestore(this.restoreBatchSize, this.restoreGapLimit, this.restoreStartCounter, keysetId);
		if (proofs.length === 0) {
			this.logger?.warn("No proofs to sweep", {
				mintUrl,
				keysetId
			});
			return;
		}
		this.logger?.debug("Proofs found for sweep", {
			mintUrl,
			keysetId,
			count: proofs.length
		});
		const states = await sweepWallet.checkProofsStates(proofs);
		if (!Array.isArray(states) || states.length !== proofs.length) {
			this.logger?.error("Malformed state check", {
				mintUrl,
				keysetId,
				statesLength: states?.length,
				proofsLength: proofs.length
			});
			throw new Error("Malformed state check");
		}
		const checkedProofs = {
			spent: [],
			ready: []
		};
		for (const [index, state] of states.entries()) {
			if (!proofs[index]) {
				this.logger?.error("Proof not found", {
					mintUrl,
					keysetId,
					index
				});
				throw new Error("Proof not found");
			}
			if (state.state === "SPENT") checkedProofs.spent.push(proofs[index]);
			else checkedProofs.ready.push(proofs[index]);
		}
		this.logger?.debug("Checked proof states", {
			mintUrl,
			keysetId,
			ready: checkedProofs.ready.length,
			spent: checkedProofs.spent.length
		});
		if (checkedProofs.ready.length === 0) {
			this.logger?.warn("No ready proofs to sweep, all spent", {
				mintUrl,
				keysetId,
				spentCount: checkedProofs.spent.length
			});
			return;
		}
		const sweepFee = sweepWallet.getFeesForProofs(checkedProofs.ready);
		const sweepAmount = checkedProofs.ready.reduce((acc, proof) => acc + proof.amount, 0);
		const sweepTotalAmount = sweepAmount - sweepFee;
		if (sweepTotalAmount < 0) {
			this.logger?.warn("Sweep amount is less than fee", {
				mintUrl,
				keysetId,
				amount: sweepAmount,
				fee: sweepFee,
				total: sweepTotalAmount
			});
			return;
		}
		this.logger?.debug("Sweep calculation", {
			mintUrl,
			keysetId,
			amount: sweepAmount,
			fee: sweepFee,
			total: sweepTotalAmount
		});
		const { send: sendData } = await this.proofService.createOutputsAndIncrementCounters(mintUrl, {
			keep: 0,
			send: sweepTotalAmount
		});
		const { send, keep } = await wallet.send(sweepTotalAmount, checkedProofs.ready, { outputData: {
			keep: [],
			send: sendData
		} });
		await this.proofService.saveProofs(mintUrl, mapProofToCoreProof(mintUrl, "ready", [...keep, ...send]));
		this.logger?.info("Keyset sweep completed", {
			mintUrl,
			keysetId,
			readyProofs: checkedProofs.ready.length,
			spentProofs: checkedProofs.spent.length,
			sweptAmount: sweepAmount,
			fee: sweepFee
		});
	}
	/**
	* Restore and persist proofs for a single keyset.
	* Enforces the invariant: restored proofs must be >= previously stored proofs.
	* Throws on any validation or persistence error. No transactions are used here.
	*/
	async restoreKeyset(mintUrl, wallet, keysetId) {
		this.logger?.debug("Restoring keyset", {
			mintUrl,
			keysetId
		});
		const oldProofs = await this.proofService.getProofsByKeysetId(mintUrl, keysetId);
		this.logger?.debug("Existing proofs before restore", {
			mintUrl,
			keysetId,
			count: oldProofs.length
		});
		const { proofs, lastCounterWithSignature } = await wallet.batchRestore(this.restoreBatchSize, this.restoreGapLimit, this.restoreStartCounter, keysetId);
		if (proofs.length === 0) {
			this.logger?.warn("No proofs to restore", {
				mintUrl,
				keysetId
			});
			return;
		}
		this.logger?.info("Batch restore result", {
			mintUrl,
			keysetId,
			restored: proofs.length,
			lastCounterWithSignature
		});
		if (oldProofs.length > proofs.length) {
			this.logger?.warn("Restored fewer proofs than previously stored", {
				mintUrl,
				keysetId,
				previous: oldProofs.length,
				restored: proofs.length
			});
			throw new Error("Restored less proofs than expected.");
		}
		const states = await wallet.checkProofsStates(proofs);
		if (!Array.isArray(states) || states.length !== proofs.length) {
			this.logger?.error("Malformed state check", {
				mintUrl,
				keysetId,
				statesLength: states?.length,
				proofsLength: proofs.length
			});
			throw new Error("Malformed state check");
		}
		const checkedProofs = {
			spent: [],
			ready: []
		};
		for (const [index, state] of states.entries()) {
			if (!proofs[index]) {
				this.logger?.error("Proof not found", {
					mintUrl,
					keysetId,
					index
				});
				throw new Error("Proof not found");
			}
			if (state.state === "SPENT") checkedProofs.spent.push(proofs[index]);
			else checkedProofs.ready.push(proofs[index]);
		}
		this.logger?.debug("Checked proof states", {
			mintUrl,
			keysetId,
			ready: checkedProofs.ready.length,
			spent: checkedProofs.spent.length
		});
		const newCounter = lastCounterWithSignature ? lastCounterWithSignature + 1 : 0;
		await this.counterService.overwriteCounter(mintUrl, keysetId, newCounter);
		this.logger?.debug("Requested counter overwrite for keyset", {
			mintUrl,
			keysetId,
			counter: newCounter
		});
		await this.proofService.saveProofs(mintUrl, mapProofToCoreProof(mintUrl, "ready", checkedProofs.ready));
		this.logger?.info("Saved restored proofs for keyset", {
			mintUrl,
			keysetId,
			total: checkedProofs.ready.length + checkedProofs.spent.length
		});
	}
};

//#endregion
//#region services/MeltQuoteService.ts
var MeltQuoteService = class {
	mintService;
	proofService;
	walletService;
	meltQuoteRepo;
	logger;
	eventBus;
	constructor(mintService, proofService, walletService, meltQuoteRepo, eventBus, logger) {
		this.mintService = mintService;
		this.proofService = proofService;
		this.walletService = walletService;
		this.meltQuoteRepo = meltQuoteRepo;
		this.eventBus = eventBus;
		this.logger = logger;
	}
	async createMeltQuote(mintUrl, invoice) {
		if (!mintUrl || !mintUrl.trim()) {
			this.logger?.warn("Invalid parameter: mintUrl is required for createMeltQuote");
			throw new Error("mintUrl is required");
		}
		if (!invoice || !invoice.trim()) {
			this.logger?.warn("Invalid parameter: invoice is required for createMeltQuote", { mintUrl });
			throw new Error("invoice is required");
		}
		if (!await this.mintService.isTrustedMint(mintUrl)) throw new UnknownMintError(`Mint ${mintUrl} is not trusted`);
		this.logger?.info("Creating melt quote", { mintUrl });
		try {
			const { wallet } = await this.walletService.getWalletWithActiveKeysetId(mintUrl);
			const quote = await wallet.createMeltQuote(invoice);
			await this.meltQuoteRepo.addMeltQuote({
				...quote,
				mintUrl
			});
			await this.eventBus.emit("melt-quote:created", {
				mintUrl,
				quoteId: quote.quote,
				quote
			});
			return quote;
		} catch (err) {
			this.logger?.error("Failed to create melt quote", {
				mintUrl,
				err
			});
			throw err;
		}
	}
	async payMeltQuote(mintUrl, quoteId) {
		if (!mintUrl || !mintUrl.trim()) {
			this.logger?.warn("Invalid parameter: mintUrl is required for payMeltQuote");
			throw new Error("mintUrl is required");
		}
		if (!quoteId || !quoteId.trim()) {
			this.logger?.warn("Invalid parameter: quoteId is required for payMeltQuote", { mintUrl });
			throw new Error("quoteId is required");
		}
		if (!await this.mintService.isTrustedMint(mintUrl)) throw new UnknownMintError(`Mint ${mintUrl} is not trusted`);
		this.logger?.info("Paying melt quote", {
			mintUrl,
			quoteId
		});
		try {
			const quote = await this.meltQuoteRepo.getMeltQuote(mintUrl, quoteId);
			if (!quote) {
				this.logger?.warn("Melt quote not found", {
					mintUrl,
					quoteId
				});
				throw new Error("Quote not found");
			}
			const { wallet } = await this.walletService.getWalletWithActiveKeysetId(mintUrl);
			let targetAmount = quote.amount + quote.fee_reserve;
			const selectedProofs = await this.proofService.selectProofsToSend(mintUrl, targetAmount);
			const selectedInputFee = wallet.getFeesForProofs(selectedProofs);
			targetAmount = targetAmount + selectedInputFee;
			const selectedAmount = selectedProofs.reduce((acc, proof) => acc + proof.amount, 0);
			if (selectedAmount < targetAmount) {
				this.logger?.warn("Insufficient proofs to cover melt amount with fee", {
					mintUrl,
					quoteId,
					required: targetAmount,
					available: selectedAmount
				});
				throw new Error("Insufficient proofs to pay melt quote");
			}
			if (selectedAmount === targetAmount) {
				this.logger?.debug("Exact amount match, skipping send/swap", {
					mintUrl,
					quoteId,
					amount: targetAmount
				});
				await this.proofService.setProofState(mintUrl, selectedProofs.map((proof) => proof.secret), "inflight");
				const { change } = await wallet.meltProofs(quote, selectedProofs);
				await this.proofService.saveProofs(mintUrl, mapProofToCoreProof(mintUrl, "ready", change));
				await this.proofService.setProofState(mintUrl, selectedProofs.map((proof) => proof.secret), "spent");
			} else {
				this.logger?.debug("Selected amount is greater than amount with fee, need to swap proofs", {
					mintUrl,
					quoteId,
					selectedAmount,
					targetAmount,
					selectedProofs
				});
				const swapFees = wallet.getFeesForProofs(selectedProofs);
				const totalSendAmount = quote.amount + quote.fee_reserve + swapFees;
				if (selectedAmount < totalSendAmount) {
					this.logger?.warn("Insufficient proofs after fee calculation", {
						mintUrl,
						quoteId,
						selectedAmount,
						totalSendAmount,
						swapFees
					});
					throw new Error("Insufficient proofs to pay melt quote after fees");
				}
				const outputData = await this.proofService.createOutputsAndIncrementCounters(mintUrl, {
					keep: selectedAmount - quote.amount - quote.fee_reserve - swapFees,
					send: quote.amount + quote.fee_reserve
				}, { includeFees: true });
				const { send, keep } = await wallet.send(outputData.sendAmount, selectedProofs, { outputData });
				this.logger?.debug("Swapped successfully", {
					mintUrl,
					quoteId,
					send,
					keep
				});
				await this.proofService.saveProofs(mintUrl, mapProofToCoreProof(mintUrl, "ready", [...keep, ...send]));
				await this.proofService.setProofState(mintUrl, selectedProofs.map((proof) => proof.secret), "spent");
				await this.proofService.setProofState(mintUrl, send.map((proof) => proof.secret), "inflight");
				const { change } = await wallet.meltProofs(quote, send);
				await this.proofService.saveProofs(mintUrl, mapProofToCoreProof(mintUrl, "ready", change));
				await this.proofService.setProofState(mintUrl, send.map((proof) => proof.secret), "spent");
			}
			await this.setMeltQuoteState(mintUrl, quoteId, "PAID");
			await this.eventBus.emit("melt-quote:paid", {
				mintUrl,
				quoteId,
				quote
			});
		} catch (err) {
			this.logger?.error("Failed to pay melt quote", {
				mintUrl,
				quoteId,
				err
			});
			throw err;
		}
	}
	async setMeltQuoteState(mintUrl, quoteId, state) {
		this.logger?.debug("Setting melt quote state", {
			mintUrl,
			quoteId,
			state
		});
		await this.meltQuoteRepo.setMeltQuoteState(mintUrl, quoteId, state);
		await this.eventBus.emit("melt-quote:state-changed", {
			mintUrl,
			quoteId,
			state
		});
		this.logger?.debug("Melt quote state updated", {
			mintUrl,
			quoteId,
			state
		});
	}
};

//#endregion
//#region services/HistoryService.ts
var HistoryService = class {
	historyRepository;
	logger;
	eventBus;
	constructor(historyRepository, eventBus, logger) {
		this.historyRepository = historyRepository;
		this.logger = logger;
		this.eventBus = eventBus;
		this.eventBus.on("mint-quote:state-changed", ({ mintUrl, quoteId, state }) => {
			this.handleMintQuoteStateChanged(mintUrl, quoteId, state);
		});
		this.eventBus.on("mint-quote:created", ({ mintUrl, quoteId, quote }) => {
			this.handleMintQuoteCreated(mintUrl, quoteId, quote);
		});
		this.eventBus.on("mint-quote:added", ({ mintUrl, quoteId, quote }) => {
			this.handleMintQuoteAdded(mintUrl, quoteId, quote);
		});
		this.eventBus.on("melt-quote:created", ({ mintUrl, quoteId, quote }) => {
			this.handleMeltQuoteCreated(mintUrl, quoteId, quote);
		});
		this.eventBus.on("melt-quote:state-changed", ({ mintUrl, quoteId, state }) => {
			this.handleMeltQuoteStateChanged(mintUrl, quoteId, state);
		});
		this.eventBus.on("send:created", ({ mintUrl, token }) => {
			this.handleSendCreated(mintUrl, token);
		});
		this.eventBus.on("receive:created", ({ mintUrl, token }) => {
			this.handleReceiveCreated(mintUrl, token);
		});
	}
	async getPaginatedHistory(offset = 0, limit = 25) {
		return this.historyRepository.getPaginatedHistoryEntries(limit, offset);
	}
	async handleSendCreated(mintUrl, token) {
		const entry = {
			type: "send",
			createdAt: Date.now(),
			unit: token.unit || "sat",
			amount: token.proofs.reduce((acc, proof) => acc + proof.amount, 0),
			mintUrl,
			token
		};
		try {
			const entryRes = await this.historyRepository.addHistoryEntry(entry);
			await this.handleHistoryUpdated(mintUrl, entryRes);
		} catch (err) {
			this.logger?.error("Failed to add send created history entry", {
				mintUrl,
				token,
				err
			});
		}
	}
	async handleReceiveCreated(mintUrl, token) {
		const entry = {
			type: "receive",
			createdAt: Date.now(),
			unit: token.unit || "sat",
			amount: token.proofs.reduce((acc, proof) => acc + proof.amount, 0),
			mintUrl
		};
		try {
			const entryRes = await this.historyRepository.addHistoryEntry(entry);
			await this.handleHistoryUpdated(mintUrl, entryRes);
		} catch (err) {
			this.logger?.error("Failed to add receive created history entry", {
				mintUrl,
				token,
				err
			});
		}
	}
	async handleMintQuoteStateChanged(mintUrl, quoteId, state) {
		try {
			const entry = await this.historyRepository.getMintHistoryEntry(mintUrl, quoteId);
			if (!entry) {
				this.logger?.error("Mint quote state changed history entry not found", {
					mintUrl,
					quoteId
				});
				return;
			}
			entry.state = state;
			await this.historyRepository.updateHistoryEntry(entry);
			await this.handleHistoryUpdated(mintUrl, {
				...entry,
				state
			});
		} catch (err) {
			this.logger?.error("Failed to add mint quote state changed history entry", {
				mintUrl,
				quoteId,
				err
			});
		}
	}
	async handleMeltQuoteStateChanged(mintUrl, quoteId, state) {
		try {
			const entry = await this.historyRepository.getMeltHistoryEntry(mintUrl, quoteId);
			if (!entry) {
				this.logger?.error("Melt quote state changed history entry not found", {
					mintUrl,
					quoteId
				});
				return;
			}
			entry.state = state;
			await this.historyRepository.updateHistoryEntry(entry);
			await this.handleHistoryUpdated(mintUrl, {
				...entry,
				state
			});
		} catch (err) {
			this.logger?.error("Failed to add melt quote state changed history entry", {
				mintUrl,
				quoteId,
				err
			});
		}
	}
	async handleMeltQuoteCreated(mintUrl, quoteId, quote) {
		const entry = {
			type: "melt",
			createdAt: Date.now(),
			unit: quote.unit,
			amount: quote.amount,
			mintUrl,
			quoteId,
			state: quote.state
		};
		try {
			await this.historyRepository.addHistoryEntry(entry);
		} catch (err) {
			this.logger?.error("Failed to add melt quote created history entry", {
				mintUrl,
				quoteId,
				err
			});
		}
	}
	async handleMintQuoteCreated(mintUrl, quoteId, quote) {
		const entry = {
			type: "mint",
			mintUrl,
			unit: quote.unit,
			paymentRequest: quote.request,
			quoteId,
			state: quote.state,
			createdAt: Date.now(),
			amount: quote.amount
		};
		try {
			await this.historyRepository.addHistoryEntry(entry);
		} catch (err) {
			this.logger?.error("Failed to add mint quote created history entry", {
				mintUrl,
				quoteId,
				err
			});
		}
	}
	async handleMintQuoteAdded(mintUrl, quoteId, quote) {
		if (await this.historyRepository.getMintHistoryEntry(mintUrl, quoteId)) {
			this.logger?.debug("History entry already exists for added mint quote", {
				mintUrl,
				quoteId
			});
			return;
		}
		const entry = {
			type: "mint",
			mintUrl,
			unit: quote.unit,
			paymentRequest: quote.request,
			quoteId,
			state: quote.state,
			createdAt: Date.now(),
			amount: quote.amount
		};
		try {
			const created = await this.historyRepository.addHistoryEntry(entry);
			await this.eventBus.emit("history:updated", {
				mintUrl,
				entry: created
			});
			this.logger?.debug("Added history entry for externally added mint quote", {
				mintUrl,
				quoteId,
				state: quote.state
			});
		} catch (err) {
			this.logger?.error("Failed to add mint quote added history entry", {
				mintUrl,
				quoteId,
				err
			});
		}
	}
	async handleHistoryUpdated(mintUrl, entry) {
		try {
			await this.eventBus.emit("history:updated", {
				mintUrl,
				entry
			});
		} catch (err) {
			this.logger?.error("Failed to emit history entry", {
				mintUrl,
				entry,
				err
			});
		}
	}
};

//#endregion
//#region services/TransactionService.ts
var TransactionService = class {
	mintService;
	walletService;
	proofService;
	eventBus;
	logger;
	constructor(mintService, walletService, proofService, eventBus, logger) {
		this.mintService = mintService;
		this.walletService = walletService;
		this.proofService = proofService;
		this.eventBus = eventBus;
		this.logger = logger;
	}
	async receive(token) {
		let mint;
		try {
			mint = typeof token === "string" ? (0, __cashu_cashu_ts.getTokenMetadata)(token).mint : token.mint;
		} catch (err) {
			this.logger?.warn("Failed to decode token for receive", { err });
			throw new ProofValidationError("Invalid token");
		}
		if (!await this.mintService.isTrustedMint(mint)) throw new UnknownMintError(`Mint ${mint} is not trusted`);
		try {
			const { keysets } = await this.mintService.ensureUpdatedMint(mint);
			const { wallet } = await this.walletService.getWalletWithActiveKeysetId(mint);
			let proofs = typeof token === "string" ? (0, __cashu_cashu_ts.getDecodedToken)(token, keysets).proofs : token.proofs;
			proofs = await this.proofService.prepareProofsForReceiving(proofs);
			if (!Array.isArray(proofs) || proofs.length === 0) {
				this.logger?.warn("Token contains no proofs", { mint });
				throw new ProofValidationError("Token contains no proofs");
			}
			const receiveAmount = proofs.reduce((acc, proof) => acc + proof.amount, 0);
			if (!Number.isFinite(receiveAmount) || receiveAmount <= 0) {
				this.logger?.warn("Token has invalid or non-positive amount", {
					mint,
					receiveAmount
				});
				throw new ProofValidationError("Token amount must be a positive integer");
			}
			this.logger?.info("Receiving token", {
				mint,
				proofs: proofs.length,
				amount: receiveAmount
			});
			const fees = wallet.getFeesForProofs(proofs);
			const { keep: outputData } = await this.proofService.createOutputsAndIncrementCounters(mint, {
				keep: receiveAmount - fees,
				send: 0
			});
			if (!outputData || outputData.length === 0) {
				this.logger?.error("Failed to create deterministic outputs for receive", {
					mint,
					amount: receiveAmount
				});
				throw new Error("Failed to create outputs for receive");
			}
			const newProofs = await wallet.receive({
				mint,
				proofs
			}, { outputData });
			await this.proofService.saveProofs(mint, mapProofToCoreProof(mint, "ready", newProofs));
			await this.eventBus.emit("receive:created", {
				mintUrl: mint,
				token: {
					mint,
					proofs
				}
			});
			this.logger?.debug("Token received and proofs saved", {
				mint,
				newProofs: newProofs.length
			});
		} catch (err) {
			this.logger?.error("Failed to receive token", {
				mint,
				err
			});
			throw err;
		}
	}
	async send(mintUrl, amount) {
		if (!await this.mintService.isTrustedMint(mintUrl)) throw new UnknownMintError(`Mint ${mintUrl} is not trusted`);
		const { wallet } = await this.walletService.getWalletWithActiveKeysetId(mintUrl);
		const exactProofs = await this.proofService.selectProofsToSend(mintUrl, amount, false);
		if (exactProofs.reduce((acc, proof) => acc + proof.amount, 0) === amount && exactProofs.length > 0) {
			this.logger?.info("Exact amount match, skipping swap", {
				mintUrl,
				amountToSend: amount,
				proofCount: exactProofs.length
			});
			await this.proofService.setProofState(mintUrl, exactProofs.map((proof) => proof.secret), "inflight");
			const token$1 = {
				mint: mintUrl,
				proofs: exactProofs
			};
			await this.eventBus.emit("send:created", {
				mintUrl,
				token: token$1
			});
			return token$1;
		}
		const selectedProofs = await this.proofService.selectProofsToSend(mintUrl, amount, true);
		const fees = wallet.getFeesForProofs(selectedProofs);
		const selectedAmount = selectedProofs.reduce((acc, proof) => acc + proof.amount, 0);
		const outputData = await this.proofService.createOutputsAndIncrementCounters(mintUrl, {
			keep: selectedAmount - amount - fees,
			send: amount
		});
		this.logger?.info("Sending with swap", {
			mintUrl,
			amountToSend: amount,
			fees,
			selectedAmount,
			proofCount: selectedProofs.length
		});
		const { send, keep } = await wallet.send(amount, selectedProofs, { outputData });
		await this.proofService.saveProofs(mintUrl, mapProofToCoreProof(mintUrl, "ready", [...keep, ...send]));
		await this.proofService.setProofState(mintUrl, selectedProofs.map((proof) => proof.secret), "spent");
		await this.proofService.setProofState(mintUrl, send.map((proof) => proof.secret), "inflight");
		const token = {
			mint: mintUrl,
			proofs: send
		};
		await this.eventBus.emit("send:created", {
			mintUrl,
			token
		});
		return token;
	}
};

//#endregion
//#region services/PaymentRequestService.ts
var PaymentRequestService = class {
	transactionService;
	logger;
	constructor(transactionService, logger) {
		this.transactionService = transactionService;
		this.logger = logger;
	}
	async readPaymentRequest(paymentRequest) {
		this.logger?.debug("Reading payment request", { paymentRequest });
		const decodedPaymentRequest = __cashu_cashu_ts.PaymentRequest.fromEncodedRequest(paymentRequest);
		if (decodedPaymentRequest.nut10) throw new PaymentRequestError("Locked tokens (NUT-10) are not supported");
		const transport = this.getPaymentRequestTransport(decodedPaymentRequest);
		const base = {
			mints: decodedPaymentRequest.mints,
			amount: decodedPaymentRequest.amount
		};
		this.logger?.info("Payment request decoded", {
			transport: transport.type,
			mints: base.mints,
			amount: base.amount
		});
		if (transport.type === "inband") return {
			...base,
			transport
		};
		return {
			...base,
			transport
		};
	}
	/**
	* Handle an inband payment request by sending tokens and calling the handler.
	* @param mintUrl - The mint to send from
	* @param request - The prepared payment request
	* @param inbandHandler - Callback to deliver the token
	* @param amount - Optional amount (required if not specified in request)
	*/
	async handleInbandPaymentRequest(mintUrl, request, inbandHandler, amount) {
		this.validateMint(mintUrl, request.mints);
		const finalAmount = this.validateAmount(request, amount);
		this.logger?.info("Handling inband payment request", {
			mintUrl,
			amount: finalAmount
		});
		const token = await this.transactionService.send(mintUrl, finalAmount);
		await inbandHandler(token);
		this.logger?.debug("Inband payment request completed", {
			mintUrl,
			amount: finalAmount
		});
	}
	/**
	* Handle an HTTP payment request by sending tokens to the specified URL.
	* @param mintUrl - The mint to send from
	* @param request - The prepared payment request
	* @param amount - Optional amount (required if not specified in request)
	* @returns The HTTP response from the payment endpoint
	*/
	async handleHttpPaymentRequest(mintUrl, request, amount) {
		this.validateMint(mintUrl, request.mints);
		const finalAmount = this.validateAmount(request, amount);
		this.logger?.info("Handling HTTP payment request", {
			mintUrl,
			amount: finalAmount,
			url: request.transport.url
		});
		const token = await this.transactionService.send(mintUrl, finalAmount);
		const response = await fetch(request.transport.url, {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify(token)
		});
		this.logger?.debug("HTTP payment request completed", {
			mintUrl,
			amount: finalAmount,
			url: request.transport.url,
			status: response.status
		});
		return response;
	}
	validateMint(mintUrl, mints) {
		if (mints && !mints.includes(mintUrl)) throw new PaymentRequestError(`Mint ${mintUrl} is not in the allowed mints list: ${mints.join(", ")}`);
	}
	getPaymentRequestTransport(pr) {
		if (!pr.transport || !Array.isArray(pr.transport)) throw new PaymentRequestError("Malformed payment request: No transport");
		if (pr.transport.length === 0) return { type: "inband" };
		const httpTransport = pr.transport.find((t) => t.type === __cashu_cashu_ts.PaymentRequestTransportType.POST);
		if (httpTransport) return {
			type: "http",
			url: httpTransport.target
		};
		const supportedTypes = pr.transport.map((t) => t.type).join(", ");
		throw new PaymentRequestError(`Unsupported transport type. Only HTTP POST is supported, found: ${supportedTypes}`);
	}
	validateAmount(request, amount) {
		if (request.amount && amount && request.amount !== amount) throw new PaymentRequestError(`Amount mismatch: request specifies ${request.amount} but ${amount} was provided`);
		const finalAmount = request.amount ?? amount;
		if (!finalAmount) throw new PaymentRequestError("Amount is required but was not provided");
		return finalAmount;
	}
};

//#endregion
//#region infra/WsConnectionManager.ts
var WsConnectionManager = class {
	sockets = /* @__PURE__ */ new Map();
	isOpenByMint = /* @__PURE__ */ new Map();
	sendQueueByMint = /* @__PURE__ */ new Map();
	logger;
	listenersByMint = /* @__PURE__ */ new Map();
	reconnectAttemptsByMint = /* @__PURE__ */ new Map();
	reconnectTimeoutByMint = /* @__PURE__ */ new Map();
	paused = false;
	constructor(wsFactory, logger) {
		this.wsFactory = wsFactory;
		this.logger = logger;
	}
	buildWsUrl(baseMintUrl) {
		const url = new URL(baseMintUrl);
		url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
		url.pathname = `${url.pathname.endsWith("/") ? url.pathname.slice(0, -1) : url.pathname}/v1/ws`;
		return url.toString();
	}
	ensureSocket(mintUrl) {
		const existing = this.sockets.get(mintUrl);
		if (existing) return existing;
		const wsUrl = this.buildWsUrl(mintUrl);
		const socket = this.wsFactory(wsUrl);
		this.sockets.set(mintUrl, socket);
		this.isOpenByMint.set(mintUrl, false);
		const onOpen = () => {
			this.isOpenByMint.set(mintUrl, true);
			const pending = this.reconnectTimeoutByMint.get(mintUrl);
			if (pending) {
				clearTimeout(pending);
				this.reconnectTimeoutByMint.delete(mintUrl);
			}
			this.reconnectAttemptsByMint.delete(mintUrl);
			const queue = this.sendQueueByMint.get(mintUrl);
			if (queue && queue.length > 0) {
				this.logger?.debug("Flushing queued messages", {
					mintUrl,
					count: queue.length
				});
				for (const payload of queue) try {
					socket.send(payload);
					this.logger?.debug("Sent queued message", {
						mintUrl,
						payloadLength: payload.length
					});
				} catch (err) {
					this.logger?.error("WS send error while flushing queue", {
						mintUrl,
						err
					});
				}
				this.sendQueueByMint.set(mintUrl, []);
			}
			this.logger?.info("WS opened", { mintUrl });
		};
		const onError = (err) => {
			this.logger?.error("WS error", {
				mintUrl,
				err
			});
		};
		const onClose = () => {
			this.logger?.info("WS closed", { mintUrl });
			this.sockets.delete(mintUrl);
			this.isOpenByMint.set(mintUrl, false);
			this.sendQueueByMint.delete(mintUrl);
			if (!this.paused) {
				const hasListeners = this.listenersByMint.get(mintUrl);
				if (hasListeners && Array.from(hasListeners.values()).some((s) => s.size > 0)) this.scheduleReconnect(mintUrl);
			}
		};
		socket.addEventListener("open", onOpen);
		socket.addEventListener("error", onError);
		socket.addEventListener("close", onClose);
		const map = this.listenersByMint.get(mintUrl);
		if (map) for (const [type, set] of map.entries()) for (const listener of set.values()) socket.addEventListener(type, listener);
		return socket;
	}
	scheduleReconnect(mintUrl) {
		if (this.reconnectTimeoutByMint.get(mintUrl)) return;
		const attempt = (this.reconnectAttemptsByMint.get(mintUrl) ?? 0) + 1;
		this.reconnectAttemptsByMint.set(mintUrl, attempt);
		const delayMs = Math.min(3e4, 1e3 * 2 ** Math.min(6, attempt - 1));
		this.logger?.info("Scheduling WS reconnect", {
			mintUrl,
			attempt,
			delayMs
		});
		const timeoutId = setTimeout(() => {
			this.reconnectTimeoutByMint.delete(mintUrl);
			try {
				this.ensureSocket(mintUrl);
			} catch (err) {
				this.logger?.error("WS reconnect attempt failed to create socket", {
					mintUrl,
					err
				});
				this.scheduleReconnect(mintUrl);
			}
		}, delayMs);
		this.reconnectTimeoutByMint.set(mintUrl, timeoutId);
	}
	on(mintUrl, type, listener) {
		const socketExists = this.sockets.has(mintUrl);
		let map = this.listenersByMint.get(mintUrl);
		if (!map) {
			map = /* @__PURE__ */ new Map();
			this.listenersByMint.set(mintUrl, map);
		}
		let set = map.get(type);
		if (!set) {
			set = /* @__PURE__ */ new Set();
			map.set(type, set);
		}
		if (set.has(listener)) return;
		set.add(listener);
		const socket = this.ensureSocket(mintUrl);
		if (socketExists) socket.addEventListener(type, listener);
	}
	off(mintUrl, type, listener) {
		this.ensureSocket(mintUrl).removeEventListener(type, listener);
		(this.listenersByMint.get(mintUrl)?.get(type))?.delete(listener);
	}
	send(mintUrl, message) {
		const socket = this.ensureSocket(mintUrl);
		const payload = typeof message === "string" ? message : JSON.stringify(message);
		if (this.isOpenByMint.get(mintUrl)) {
			try {
				socket.send(payload);
				this.logger?.debug("Sent message immediately (socket open)", {
					mintUrl,
					payloadLength: payload.length
				});
			} catch (err) {
				this.logger?.error("WS send error", {
					mintUrl,
					err
				});
			}
			return;
		}
		let queue = this.sendQueueByMint.get(mintUrl);
		if (!queue) {
			queue = [];
			this.sendQueueByMint.set(mintUrl, queue);
		}
		queue.push(payload);
		this.logger?.debug("Queued message (socket not open)", {
			mintUrl,
			queueLength: queue.length,
			payloadLength: payload.length
		});
	}
	closeAll() {
		for (const [mintUrl, socket] of this.sockets.entries()) try {
			socket.close(1e3, "Normal Closure");
		} catch (err) {
			this.logger?.warn("Error while closing WS", {
				mintUrl,
				err
			});
		}
		this.sockets.clear();
		this.isOpenByMint.clear();
		this.sendQueueByMint.clear();
		for (const timeout of this.reconnectTimeoutByMint.values()) clearTimeout(timeout);
		this.reconnectTimeoutByMint.clear();
		this.reconnectAttemptsByMint.clear();
	}
	closeMint(mintUrl) {
		const socket = this.sockets.get(mintUrl);
		if (socket) {
			try {
				socket.close(1e3, "Mint closed");
				this.logger?.debug("WS closed for mint", { mintUrl });
			} catch (err) {
				this.logger?.warn("Error while closing WS for mint", {
					mintUrl,
					err
				});
			}
			this.sockets.delete(mintUrl);
		}
		this.isOpenByMint.delete(mintUrl);
		this.sendQueueByMint.delete(mintUrl);
		this.listenersByMint.delete(mintUrl);
		const timeout = this.reconnectTimeoutByMint.get(mintUrl);
		if (timeout) {
			clearTimeout(timeout);
			this.reconnectTimeoutByMint.delete(mintUrl);
		}
		this.reconnectAttemptsByMint.delete(mintUrl);
		this.logger?.info("WsConnectionManager closed mint", { mintUrl });
	}
	pause() {
		this.paused = true;
		for (const timeout of this.reconnectTimeoutByMint.values()) clearTimeout(timeout);
		this.reconnectTimeoutByMint.clear();
		this.reconnectAttemptsByMint.clear();
		for (const [mintUrl, socket] of this.sockets.entries()) try {
			socket.close(1e3, "Paused");
			this.logger?.debug("WS closed for pause", { mintUrl });
		} catch (err) {
			this.logger?.warn("Error while closing WS for pause", {
				mintUrl,
				err
			});
		}
		this.sockets.clear();
		this.isOpenByMint.clear();
		this.sendQueueByMint.clear();
		this.logger?.info("WsConnectionManager paused");
	}
	resume() {
		this.paused = false;
		for (const [mintUrl, listenerMap] of this.listenersByMint.entries()) if (Array.from(listenerMap.values()).some((s) => s.size > 0)) try {
			this.ensureSocket(mintUrl);
			this.logger?.debug("WS reconnecting after resume", { mintUrl });
		} catch (err) {
			this.logger?.error("Failed to reconnect WS after resume", {
				mintUrl,
				err
			});
		}
		this.logger?.info("WsConnectionManager resumed");
	}
};

//#endregion
//#region infra/WsTransport.ts
var WsTransport = class {
	ws;
	constructor(wsFactoryOrManager, logger) {
		this.ws = typeof wsFactoryOrManager === "function" ? new WsConnectionManager(wsFactoryOrManager, logger) : wsFactoryOrManager;
	}
	on(mintUrl, event, handler) {
		this.ws.on(mintUrl, event, handler);
	}
	send(mintUrl, req) {
		this.ws.send(mintUrl, req);
	}
	closeAll() {
		this.ws.closeAll();
	}
	closeMint(mintUrl) {
		this.ws.closeMint(mintUrl);
	}
	pause() {
		this.ws.pause();
	}
	resume() {
		this.ws.resume();
	}
};

//#endregion
//#region infra/PollingTransport.ts
var PollingTransport = class {
	logger;
	mintAdapter;
	options;
	listenersByMint = /* @__PURE__ */ new Map();
	schedByMint = /* @__PURE__ */ new Map();
	proofQueueByMint = /* @__PURE__ */ new Map();
	proofSetByMint = /* @__PURE__ */ new Map();
	yToSubsByMint = /* @__PURE__ */ new Map();
	subToYsByMint = /* @__PURE__ */ new Map();
	paused = false;
	constructor(options, logger) {
		this.logger = logger;
		this.mintAdapter = new MintAdapter();
		this.options = { intervalMs: options?.intervalMs ?? 5e3 };
	}
	on(mintUrl, event, handler) {
		let map = this.listenersByMint.get(mintUrl);
		if (!map) {
			map = /* @__PURE__ */ new Map();
			this.listenersByMint.set(mintUrl, map);
		}
		let set = map.get(event);
		if (!set) {
			set = /* @__PURE__ */ new Set();
			map.set(event, set);
		}
		if (!set.has(handler)) set.add(handler);
		if (event === "open") {
			if (!((map.get("open")?.size ?? 0) > 0)) queueMicrotask(() => {
				try {
					handler({ type: "open" });
				} catch {}
			});
		}
		this.ensureScheduler(mintUrl);
	}
	send(mintUrl, req) {
		if (req.method === "subscribe") {
			const params = req.params;
			const subId = params.subId;
			const scheduler = this.ensureScheduler(mintUrl);
			if (params.kind === "proof_state") {
				const ys = params.filters || [];
				if (!ys.length) this.logger?.error("PollingTransport: subscribe proof_state with no filters", {
					mintUrl,
					req
				});
				let yToSubs = this.yToSubsByMint.get(mintUrl);
				if (!yToSubs) {
					yToSubs = /* @__PURE__ */ new Map();
					this.yToSubsByMint.set(mintUrl, yToSubs);
				}
				let subToYs = this.subToYsByMint.get(mintUrl);
				if (!subToYs) {
					subToYs = /* @__PURE__ */ new Map();
					this.subToYsByMint.set(mintUrl, subToYs);
				}
				let q = this.proofQueueByMint.get(mintUrl);
				if (!q) {
					q = [];
					this.proofQueueByMint.set(mintUrl, q);
				}
				let set = this.proofSetByMint.get(mintUrl);
				if (!set) {
					set = /* @__PURE__ */ new Set();
					this.proofSetByMint.set(mintUrl, set);
				}
				let subYs = subToYs.get(subId);
				if (!subYs) {
					subYs = /* @__PURE__ */ new Set();
					subToYs.set(subId, subYs);
				}
				for (const y of ys) {
					subYs.add(y);
					let subs = yToSubs.get(y);
					if (!subs) {
						subs = /* @__PURE__ */ new Set();
						yToSubs.set(y, subs);
					}
					subs.add(subId);
					if (!set.has(y)) {
						set.add(y);
						q.push(y);
					}
				}
				if (!scheduler.hasProofBatchTask) {
					scheduler.queue.push({
						kind: "proof_state",
						batch: true
					});
					scheduler.hasProofBatchTask = true;
				}
			} else {
				const filter = params.filters[0];
				if (!filter) {
					this.logger?.error("PollingTransport: subscribe with no filter", {
						mintUrl,
						req
					});
					return;
				}
				scheduler.queue.push({
					subId,
					kind: params.kind,
					filter
				});
			}
			const resp = {
				jsonrpc: "2.0",
				result: {
					status: "OK",
					subId
				},
				id: req.id
			};
			this.emit(mintUrl, "message", { data: JSON.stringify(resp) });
			this.maybeRun(mintUrl);
			return;
		}
		if (req.method === "unsubscribe") {
			const subId = req.params.subId;
			const scheduler = this.ensureScheduler(mintUrl);
			scheduler.queue = scheduler.queue.filter((t) => t.subId !== subId);
			const subToYs = this.subToYsByMint.get(mintUrl);
			const yToSubs = this.yToSubsByMint.get(mintUrl);
			const q = this.proofQueueByMint.get(mintUrl);
			const set = this.proofSetByMint.get(mintUrl);
			if (subToYs && yToSubs) {
				const ys = subToYs.get(subId);
				if (ys) {
					for (const y of ys) {
						const subs = yToSubs.get(y);
						if (subs) {
							subs.delete(subId);
							if (subs.size === 0) {
								yToSubs.delete(y);
								if (set) set.delete(y);
								if (q) {
									const idx = q.indexOf(y);
									if (idx >= 0) q.splice(idx, 1);
								}
							}
						}
					}
					subToYs.delete(subId);
				}
				if (yToSubs.size === 0 && scheduler.hasProofBatchTask) {
					scheduler.queue = scheduler.queue.filter((t) => !(t.kind === "proof_state" && t.batch));
					scheduler.hasProofBatchTask = false;
				}
			}
			return;
		}
	}
	closeAll() {
		this.schedByMint.clear();
		this.listenersByMint.clear();
		this.proofQueueByMint.clear();
		this.proofSetByMint.clear();
		this.yToSubsByMint.clear();
		this.subToYsByMint.clear();
	}
	closeMint(mintUrl) {
		this.schedByMint.delete(mintUrl);
		this.listenersByMint.delete(mintUrl);
		this.proofQueueByMint.delete(mintUrl);
		this.proofSetByMint.delete(mintUrl);
		this.yToSubsByMint.delete(mintUrl);
		this.subToYsByMint.delete(mintUrl);
		this.logger?.info("PollingTransport closed mint", { mintUrl });
	}
	pause() {
		this.paused = true;
		this.logger?.info("PollingTransport paused");
	}
	resume() {
		this.paused = false;
		for (const mintUrl of this.schedByMint.keys()) this.maybeRun(mintUrl);
		this.logger?.info("PollingTransport resumed");
	}
	ensureScheduler(mintUrl) {
		let s = this.schedByMint.get(mintUrl);
		if (!s) {
			s = {
				nextAllowedAt: 0,
				queue: [],
				running: false,
				hasProofBatchTask: false
			};
			this.schedByMint.set(mintUrl, s);
			if (!this.proofQueueByMint.get(mintUrl)) this.proofQueueByMint.set(mintUrl, []);
			if (!this.proofSetByMint.get(mintUrl)) this.proofSetByMint.set(mintUrl, /* @__PURE__ */ new Set());
			if (!this.yToSubsByMint.get(mintUrl)) this.yToSubsByMint.set(mintUrl, /* @__PURE__ */ new Map());
			if (!this.subToYsByMint.get(mintUrl)) this.subToYsByMint.set(mintUrl, /* @__PURE__ */ new Map());
		}
		return s;
	}
	async maybeRun(mintUrl) {
		if (this.paused) return;
		const s = this.ensureScheduler(mintUrl);
		if (s.running) return;
		if (Date.now() < s.nextAllowedAt) return;
		if (s.queue.length === 0) return;
		s.running = true;
		try {
			const task = s.queue.shift();
			await this.performTask(mintUrl, task);
			s.queue.push(task);
		} catch (err) {
			this.logger?.error("Polling task error", {
				mintUrl,
				err
			});
		} finally {
			s.nextAllowedAt = Date.now() + this.options.intervalMs;
			s.running = false;
			const delay = Math.max(0, s.nextAllowedAt - Date.now());
			setTimeout(() => {
				this.maybeRun(mintUrl);
			}, delay);
		}
	}
	async performTask(mintUrl, task) {
		if (task.kind === "proof_state" && task.batch) {
			const yToSubs = this.yToSubsByMint.get(mintUrl) ?? /* @__PURE__ */ new Map();
			const queue = this.proofQueueByMint.get(mintUrl) ?? [];
			if (queue.length === 0 || yToSubs.size === 0) return;
			const selected = [];
			while (selected.length < 100 && queue.length > 0) {
				const y = queue.shift();
				const subs = yToSubs.get(y);
				if (subs && subs.size > 0) {
					selected.push(y);
					queue.push(y);
				} else {
					const set = this.proofSetByMint.get(mintUrl);
					if (set) set.delete(y);
				}
			}
			if (selected.length === 0) return;
			const results = await this.mintAdapter.checkProofStates(mintUrl, selected);
			for (let i = 0; i < results.length; i++) {
				const payload$1 = results[i];
				const y = (payload$1 && typeof payload$1.Y === "string" ? payload$1.Y : void 0) ?? selected[i] ?? "";
				if (!y) continue;
				const subs = yToSubs.get(y);
				if (!subs) continue;
				for (const subId of subs.values()) {
					const notification$1 = {
						jsonrpc: "2.0",
						method: "subscribe",
						params: {
							subId,
							payload: payload$1
						}
					};
					this.emit(mintUrl, "message", { data: JSON.stringify(notification$1) });
				}
			}
			return;
		}
		let payload;
		switch (task.kind) {
			case "bolt11_mint_quote":
				payload = await this.mintAdapter.checkMintQuoteState(mintUrl, task.filter);
				break;
			case "bolt11_melt_quote":
				payload = await this.mintAdapter.checkMeltQuoteState(mintUrl, task.filter);
				break;
			default: return;
		}
		const notification = {
			jsonrpc: "2.0",
			method: "subscribe",
			params: {
				subId: task.subId,
				payload
			}
		};
		this.emit(mintUrl, "message", { data: JSON.stringify(notification) });
	}
	emit(mintUrl, event, evt) {
		const set = this.listenersByMint.get(mintUrl)?.get(event);
		if (!set) return;
		for (const handler of set.values()) try {
			handler(evt);
		} catch {}
	}
};

//#endregion
//#region infra/SubscriptionManager.ts
var SubscriptionManager = class {
	nextIdByMint = /* @__PURE__ */ new Map();
	subscriptions = /* @__PURE__ */ new Map();
	activeByMint = /* @__PURE__ */ new Map();
	pendingSubscribeByMint = /* @__PURE__ */ new Map();
	transportByMint = /* @__PURE__ */ new Map();
	logger;
	messageHandlerByMint = /* @__PURE__ */ new Map();
	openHandlerByMint = /* @__PURE__ */ new Map();
	hasOpenedByMint = /* @__PURE__ */ new Map();
	wsFactory;
	capabilitiesProvider;
	paused = false;
	constructor(wsFactoryOrManager, logger, capabilitiesProvider) {
		this.logger = logger;
		this.capabilitiesProvider = capabilitiesProvider;
		if (typeof wsFactoryOrManager === "function") this.wsFactory = wsFactoryOrManager;
		else {
			const injected = wsFactoryOrManager;
			this.transportByMint.set("*", injected);
		}
	}
	getTransport(mintUrl) {
		const injected = this.transportByMint.get("*");
		if (injected) return injected;
		let t = this.transportByMint.get(mintUrl);
		if (t) return t;
		if (this.isWebSocketAvailable() && this.wsFactory) t = new WsTransport(this.wsFactory, this.logger);
		else t = new PollingTransport({ intervalMs: 5e3 }, this.logger);
		this.transportByMint.set(mintUrl, t);
		return t;
	}
	isWebSocketAvailable() {
		return typeof globalThis.WebSocket !== "undefined" || !!this.wsFactory;
	}
	getNextId(mintUrl) {
		const next = (this.nextIdByMint.get(mintUrl) ?? 0) + 1;
		this.nextIdByMint.set(mintUrl, next);
		return next;
	}
	ensureMessageListener(mintUrl) {
		if (this.messageHandlerByMint.has(mintUrl)) return;
		const handler = (evt) => {
			try {
				const data = typeof evt.data === "string" ? evt.data : evt.data?.toString?.();
				if (!data) return;
				const parsed = JSON.parse(data);
				this.logger?.debug("Received WS message", {
					mintUrl,
					hasMethod: "method" in parsed,
					method: "method" in parsed ? parsed.method : void 0,
					hasId: "id" in parsed,
					id: "id" in parsed ? parsed.id : void 0,
					hasResult: "result" in parsed,
					hasError: "error" in parsed
				});
				if ("method" in parsed && parsed.method === "subscribe") {
					const subId = parsed.params?.subId;
					const active = subId ? this.subscriptions.get(subId) : void 0;
					if (active) for (const cb of active.callbacks) Promise.resolve(cb(parsed.params.payload)).catch((err) => this.logger?.error("Subscription callback error", {
						mintUrl,
						subId,
						err
					}));
				} else if ("error" in parsed && parsed.error) {
					const resp = parsed;
					const respId = Number(resp.id);
					const err = resp.error;
					const pendingMap = this.pendingSubscribeByMint.get(mintUrl);
					const maybeSubId = Number.isFinite(respId) && pendingMap ? pendingMap.get(respId) : void 0;
					if (maybeSubId) {
						this.subscriptions.delete(maybeSubId);
						pendingMap?.delete(respId);
						this.logger?.error("Subscribe request rejected", {
							mintUrl,
							id: resp.id,
							subId: maybeSubId,
							code: err.code,
							message: err.message
						});
					} else this.logger?.error("WS request error", {
						mintUrl,
						id: resp.id,
						code: err.code,
						message: err.message
					});
				} else if ("result" in parsed && parsed.result) {
					const resp = parsed;
					const respId = Number(resp.id);
					const pendingMap = this.pendingSubscribeByMint.get(mintUrl);
					if (Number.isFinite(respId) && pendingMap && pendingMap.has(respId)) {
						const subId = pendingMap.get(respId);
						pendingMap.delete(respId);
						this.logger?.info("Subscribe request accepted", {
							mintUrl,
							id: resp.id,
							subId: subId || resp.result?.subId
						});
					} else this.logger?.debug("Unmatched subscribe response", {
						mintUrl,
						id: resp.id,
						respId,
						hasPendingMap: !!pendingMap,
						pendingMapSize: pendingMap?.size ?? 0
					});
				}
			} catch (err) {
				this.logger?.error("WS message handling error", {
					mintUrl,
					err
				});
			}
		};
		this.getTransport(mintUrl).on(mintUrl, "message", handler);
		this.messageHandlerByMint.set(mintUrl, handler);
		const onOpen = (_evt) => {
			try {
				if (this.hasOpenedByMint.get(mintUrl) === true) {
					this.logger?.info("WS open detected, re-subscribing active subscriptions", { mintUrl });
					this.reSubscribeMint(mintUrl);
				} else {
					this.hasOpenedByMint.set(mintUrl, true);
					this.logger?.info("WS open detected, initial open - skipping re-subscribe", { mintUrl });
				}
			} catch (err) {
				this.logger?.error("Failed to handle open event", {
					mintUrl,
					err
				});
			}
		};
		this.getTransport(mintUrl).on(mintUrl, "open", onOpen);
		this.openHandlerByMint.set(mintUrl, onOpen);
	}
	async subscribe(mintUrl, kind, filters, onNotification) {
		if (!filters || filters.length === 0) throw new Error("filters must be a non-empty array");
		this.ensureMessageListener(mintUrl);
		const filtersKey = JSON.stringify([...filters].sort());
		for (const [existingSubId, existingSub] of this.subscriptions.entries()) if (existingSub.mintUrl === mintUrl && existingSub.kind === kind && JSON.stringify([...existingSub.filters].sort()) === filtersKey) {
			if (onNotification) {
				existingSub.callbacks.add(onNotification);
				this.logger?.debug("Reusing existing subscription", {
					mintUrl,
					kind,
					subId: existingSubId,
					filterCount: filters.length
				});
			}
			return {
				subId: existingSubId,
				unsubscribe: async () => {
					if (onNotification) this.removeCallback(existingSubId, onNotification);
					if (existingSub.callbacks.size === 0) await this.unsubscribe(mintUrl, existingSubId);
				}
			};
		}
		const id = this.getNextId(mintUrl);
		const subId = generateSubId();
		const req = {
			jsonrpc: "2.0",
			method: "subscribe",
			params: {
				kind,
				subId,
				filters
			},
			id
		};
		const active = {
			subId,
			mintUrl,
			kind,
			filters,
			callbacks: /* @__PURE__ */ new Set()
		};
		if (onNotification) active.callbacks.add(onNotification);
		this.subscriptions.set(subId, active);
		let set = this.activeByMint.get(mintUrl);
		if (!set) {
			set = /* @__PURE__ */ new Set();
			this.activeByMint.set(mintUrl, set);
		}
		set.add(subId);
		let pendingById = this.pendingSubscribeByMint.get(mintUrl);
		if (!pendingById) {
			pendingById = /* @__PURE__ */ new Map();
			this.pendingSubscribeByMint.set(mintUrl, pendingById);
		}
		pendingById.set(id, subId);
		if (this.paused) {
			this.logger?.info("Subscription created while paused, will activate on resume", {
				mintUrl,
				kind,
				subId
			});
			return {
				subId,
				unsubscribe: async () => {
					await this.unsubscribe(mintUrl, subId);
				}
			};
		}
		const t = this.getTransport(mintUrl);
		if (this.capabilitiesProvider) this.capabilitiesProvider.getMintInfo(mintUrl).then((info) => {
			if (!this.isMintWsSupported(info)) this.transportByMint.set(mintUrl, new PollingTransport({ intervalMs: 5e3 }, this.logger));
		}).catch(() => void 0);
		this.logger?.debug("Sending subscribe request", {
			mintUrl,
			kind,
			subId,
			id,
			filterCount: filters.length
		});
		t.send(mintUrl, req);
		this.logger?.info("Subscribed to NUT-17", {
			mintUrl,
			kind,
			subId,
			filterCount: filters.length
		});
		return {
			subId,
			unsubscribe: async () => {
				await this.unsubscribe(mintUrl, subId);
			}
		};
	}
	addCallback(subId, cb) {
		const active = this.subscriptions.get(subId);
		if (!active) throw new Error("Subscription not found");
		active.callbacks.add(cb);
	}
	removeCallback(subId, cb) {
		const active = this.subscriptions.get(subId);
		if (!active) return;
		active.callbacks.delete(cb);
	}
	async unsubscribe(mintUrl, subId) {
		const id = this.getNextId(mintUrl);
		const req = {
			jsonrpc: "2.0",
			method: "unsubscribe",
			params: { subId },
			id
		};
		this.getTransport(mintUrl).send(mintUrl, req);
		this.subscriptions.delete(subId);
		this.activeByMint.get(mintUrl)?.delete(subId);
		this.logger?.info("Unsubscribed from NUT-17", {
			mintUrl,
			subId
		});
	}
	closeAll() {
		const seen = /* @__PURE__ */ new Set();
		for (const t of this.transportByMint.values()) {
			if (seen.has(t)) continue;
			seen.add(t);
			t.closeAll();
		}
		this.subscriptions.clear();
		this.activeByMint.clear();
		this.pendingSubscribeByMint.clear();
		this.hasOpenedByMint.clear();
	}
	closeMint(mintUrl) {
		this.logger?.info("Closing all subscriptions for mint", { mintUrl });
		const subIds = this.activeByMint.get(mintUrl);
		if (subIds) for (const subId of subIds) this.subscriptions.delete(subId);
		this.activeByMint.delete(mintUrl);
		this.pendingSubscribeByMint.delete(mintUrl);
		this.nextIdByMint.delete(mintUrl);
		this.messageHandlerByMint.delete(mintUrl);
		this.openHandlerByMint.delete(mintUrl);
		this.hasOpenedByMint.delete(mintUrl);
		const transport = this.transportByMint.get(mintUrl);
		if (transport) {
			transport.closeMint(mintUrl);
			this.transportByMint.delete(mintUrl);
		}
		this.logger?.info("SubscriptionManager closed mint", { mintUrl });
	}
	reSubscribeMint(mintUrl) {
		const set = this.activeByMint.get(mintUrl);
		if (!set || set.size === 0) return;
		for (const subId of set) {
			const active = this.subscriptions.get(subId);
			if (!active) continue;
			const id = this.getNextId(mintUrl);
			const req = {
				jsonrpc: "2.0",
				method: "subscribe",
				params: {
					kind: active.kind,
					subId: active.subId,
					filters: active.filters
				},
				id
			};
			let pendingById = this.pendingSubscribeByMint.get(mintUrl);
			if (!pendingById) {
				pendingById = /* @__PURE__ */ new Map();
				this.pendingSubscribeByMint.set(mintUrl, pendingById);
			}
			pendingById.set(id, subId);
			this.getTransport(mintUrl).send(mintUrl, req);
			this.logger?.info("Re-subscribed to NUT-17 after reconnect", {
				mintUrl,
				kind: active.kind,
				subId: active.subId,
				filterCount: active.filters.length
			});
		}
	}
	isMintWsSupported(_info) {
		if (_info.nuts[17]) {
			const supported = _info.nuts[17].supported;
			const requiredKinds = [
				"bolt11_melt_quote",
				"proof_state",
				"bolt11_mint_quote"
			];
			for (const s of supported) if (s.unit === "sat") {
				const supportedKinds = new Set(s.commands);
				return requiredKinds.every((required) => supportedKinds.has(required));
			}
		}
		return false;
	}
	pause() {
		this.paused = true;
		const seen = /* @__PURE__ */ new Set();
		for (const t of this.transportByMint.values()) {
			if (seen.has(t)) continue;
			seen.add(t);
			t.pause();
		}
		this.logger?.info("SubscriptionManager paused");
	}
	resume() {
		this.paused = false;
		const seen = /* @__PURE__ */ new Set();
		for (const t of this.transportByMint.values()) {
			if (seen.has(t)) continue;
			seen.add(t);
			t.resume();
		}
		this.logger?.info("SubscriptionManager resumed");
	}
};

//#endregion
//#region logging/ConsoleLogger.ts
var ConsoleLogger = class ConsoleLogger {
	prefix;
	level;
	static levelPriority = {
		error: 0,
		warn: 1,
		info: 2,
		debug: 3
	};
	constructor(prefix = "coco-cashu", options = {}) {
		this.prefix = prefix;
		this.level = options.level ?? "info";
	}
	shouldLog(level) {
		return ConsoleLogger.levelPriority[level] <= ConsoleLogger.levelPriority[this.level];
	}
	error(message, ...meta) {
		if (!this.shouldLog("error")) return;
		console.error(`[${this.prefix}] ERROR: ${message}`, ...meta);
	}
	warn(message, ...meta) {
		if (!this.shouldLog("warn")) return;
		console.warn(`[${this.prefix}] WARN: ${message}`, ...meta);
	}
	info(message, ...meta) {
		if (!this.shouldLog("info")) return;
		console.info(`[${this.prefix}] INFO: ${message}`, ...meta);
	}
	debug(message, ...meta) {
		if (!this.shouldLog("debug")) return;
		console.debug(`[${this.prefix}] DEBUG: ${message}`, ...meta);
	}
	log(level, message, ...meta) {
		switch (level) {
			case "error":
				this.error(message, ...meta);
				break;
			case "warn":
				this.warn(message, ...meta);
				break;
			case "info":
				this.info(message, ...meta);
				break;
			case "debug":
				this.debug(message, ...meta);
				break;
			default: this.info(message, ...meta);
		}
	}
	child(bindings) {
		const name = [this.prefix, ...Object.entries(bindings).map(([k, v]) => `${k}=${String(v)}`)].join(" ");
		return new ConsoleLogger(name, { level: this.level });
	}
};

//#endregion
//#region logging/NullLogger.ts
var NullLogger = class {
	error(_message, ..._meta) {}
	warn(_message, ..._meta) {}
	info(_message, ..._meta) {}
	debug(_message, ..._meta) {}
	log(_level, _message, ..._meta) {}
	child(_bindings) {
		return this;
	}
};

//#endregion
//#region api/WalletApi.ts
var WalletApi = class {
	mintService;
	walletService;
	proofService;
	walletRestoreService;
	transactionService;
	paymentRequestService;
	logger;
	constructor(mintService, walletService, proofService, walletRestoreService, transactionService, paymentRequestService, logger) {
		this.mintService = mintService;
		this.walletService = walletService;
		this.proofService = proofService;
		this.walletRestoreService = walletRestoreService;
		this.transactionService = transactionService;
		this.paymentRequestService = paymentRequestService;
		this.logger = logger;
	}
	async receive(token) {
		return this.transactionService.receive(token);
	}
	async send(mintUrl, amount) {
		return this.transactionService.send(mintUrl, amount);
	}
	async getBalances() {
		return this.proofService.getBalances();
	}
	/**
	* Parse and validate a payment request string.
	*/
	async readPaymentRequest(paymentRequest) {
		return this.paymentRequestService.readPaymentRequest(paymentRequest);
	}
	/**
	* Handle an inband payment request by sending tokens and calling the handler.
	* @param mintUrl - The mint to send from
	* @param request - The prepared payment request (from readPaymentRequest)
	* @param inbandHandler - Callback to deliver the token (e.g., display QR, send via NFC)
	* @param amount - Optional amount (required if not specified in request)
	*/
	async handleInbandPaymentRequest(mintUrl, request, inbandHandler, amount) {
		return this.paymentRequestService.handleInbandPaymentRequest(mintUrl, request, inbandHandler, amount);
	}
	/**
	* Handle an HTTP payment request by sending tokens to the specified URL.
	* @param mintUrl - The mint to send from
	* @param request - The prepared payment request (from readPaymentRequest)
	* @param amount - Optional amount (required if not specified in request)
	* @returns The HTTP response from the payment endpoint
	*/
	async handleHttpPaymentRequest(mintUrl, request, amount) {
		return this.paymentRequestService.handleHttpPaymentRequest(mintUrl, request, amount);
	}
	async restore(mintUrl) {
		this.logger?.info("Starting restore", { mintUrl });
		const mint = await this.mintService.addMintByUrl(mintUrl, { trusted: true });
		this.logger?.debug("Mint fetched for restore", {
			mintUrl,
			keysetCount: mint.keysets.length
		});
		const { wallet } = await this.walletService.getWalletWithActiveKeysetId(mintUrl);
		const failedKeysetIds = {};
		for (const keyset of mint.keysets) try {
			await this.walletRestoreService.restoreKeyset(mintUrl, wallet, keyset.id);
		} catch (error) {
			this.logger?.error("Keyset restore failed", {
				mintUrl,
				keysetId: keyset.id,
				error
			});
			failedKeysetIds[keyset.id] = error;
		}
		if (Object.keys(failedKeysetIds).length > 0) {
			this.logger?.error("Restore completed with failures", {
				mintUrl,
				failedKeysetIds: Object.keys(failedKeysetIds)
			});
			throw new Error("Failed to restore some keysets");
		}
		this.logger?.info("Restore completed successfully", { mintUrl });
	}
	/**
	* Sweeps a mint by sweeping each keyset and adds the swept proofs to the wallet
	* @param mintUrl - The URL of the mint to sweep
	* @param bip39seed - The BIP39 seed of the wallet to sweep
	*/
	async sweep(mintUrl, bip39seed) {
		this.logger?.info("Starting sweep", { mintUrl });
		const mint = await this.mintService.addMintByUrl(mintUrl, { trusted: true });
		this.logger?.debug("Mint fetched for sweep", {
			mintUrl,
			keysetCount: mint.keysets.length
		});
		const failedKeysetIds = {};
		for (const keyset of mint.keysets) try {
			await this.walletRestoreService.sweepKeyset(mintUrl, keyset.id, bip39seed);
		} catch (error) {
			this.logger?.error("Keyset restore failed", {
				mintUrl,
				keysetId: keyset.id,
				error
			});
			failedKeysetIds[keyset.id] = error;
		}
		if (Object.keys(failedKeysetIds).length > 0) {
			this.logger?.error("Restore completed with failures", {
				mintUrl,
				failedKeysetIds: Object.keys(failedKeysetIds)
			});
			throw new Error("Failed to restore some keysets");
		}
		this.logger?.info("Restore completed successfully", { mintUrl });
	}
};

//#endregion
//#region api/QuotesApi.ts
var QuotesApi = class {
	mintQuoteService;
	meltQuoteService;
	constructor(mintQuoteService, meltQuoteService) {
		this.mintQuoteService = mintQuoteService;
		this.meltQuoteService = meltQuoteService;
	}
	async createMintQuote(mintUrl, amount) {
		return this.mintQuoteService.createMintQuote(mintUrl, amount);
	}
	async redeemMintQuote(mintUrl, quoteId) {
		return this.mintQuoteService.redeemMintQuote(mintUrl, quoteId);
	}
	async createMeltQuote(mintUrl, invoice) {
		return this.meltQuoteService.createMeltQuote(mintUrl, invoice);
	}
	async payMeltQuote(mintUrl, quoteId) {
		return this.meltQuoteService.payMeltQuote(mintUrl, quoteId);
	}
	async addMintQuote(mintUrl, quotes) {
		return this.mintQuoteService.addExistingMintQuotes(mintUrl, quotes);
	}
	async requeuePaidMintQuotes(mintUrl) {
		return this.mintQuoteService.requeuePaidMintQuotes(mintUrl);
	}
};

//#endregion
//#region api/MintApi.ts
var MintApi = class {
	constructor(mintService) {
		this.mintService = mintService;
	}
	async addMint(mintUrl, options) {
		return this.mintService.addMintByUrl(mintUrl, options);
	}
	async getMintInfo(mintUrl) {
		return this.mintService.getMintInfo(mintUrl);
	}
	async isTrustedMint(mintUrl) {
		return this.mintService.isTrustedMint(mintUrl);
	}
	async getAllMints() {
		return this.mintService.getAllMints();
	}
	async getAllTrustedMints() {
		return this.mintService.getAllTrustedMints();
	}
	async trustMint(mintUrl) {
		return this.mintService.trustMint(mintUrl);
	}
	async untrustMint(mintUrl) {
		return this.mintService.untrustMint(mintUrl);
	}
};

//#endregion
//#region api/KeyRingApi.ts
var KeyRingApi = class {
	constructor(keyRingService) {
		this.keyRingService = keyRingService;
	}
	async generateKeyPair(dumpSecretKey) {
		if (dumpSecretKey === true) return this.keyRingService.generateNewKeyPair({ dumpSecretKey: true });
		return this.keyRingService.generateNewKeyPair({ dumpSecretKey: false });
	}
	/**
	* Adds an existing keypair to the keyring using a secret key.
	* @param secretKey - The 32-byte secret key as Uint8Array
	*/
	async addKeyPair(secretKey) {
		return this.keyRingService.addKeyPair(secretKey);
	}
	/**
	* Removes a keypair from the keyring.
	* @param publicKey - The public key (hex string) of the keypair to remove
	*/
	async removeKeyPair(publicKey) {
		return this.keyRingService.removeKeyPair(publicKey);
	}
	/**
	* Retrieves a specific keypair by its public key.
	* @param publicKey - The public key (hex string) to look up
	* @returns The keypair if found, null otherwise
	*/
	async getKeyPair(publicKey) {
		return this.keyRingService.getKeyPair(publicKey);
	}
	/**
	* Gets the most recently added keypair.
	* @returns The latest keypair if any exist, null otherwise
	*/
	async getLatestKeyPair() {
		return this.keyRingService.getLatestKeyPair();
	}
	/**
	* Gets all keypairs stored in the keyring.
	* @returns Array of all keypairs
	*/
	async getAllKeyPairs() {
		return this.keyRingService.getAllKeyPairs();
	}
};

//#endregion
//#region api/SubscriptionApi.ts
var SubscriptionApi = class {
	subs;
	logger;
	constructor(subs, logger) {
		this.subs = subs;
		this.logger = logger;
	}
	async awaitMintQuotePaid(mintUrl, quoteId) {
		return this.awaitFirstNotification(mintUrl, "bolt11_mint_quote", [quoteId]);
	}
	async awaitMeltQuotePaid(mintUrl, quoteId) {
		return this.awaitFirstNotification(mintUrl, "bolt11_melt_quote", [quoteId]);
	}
	async awaitFirstNotification(mintUrl, kind, filters) {
		return new Promise(async (resolve, reject) => {
			try {
				const { unsubscribe } = await this.subs.subscribe(mintUrl, kind, filters, (payload) => {
					try {
						resolve(payload);
					} finally {
						unsubscribe().catch(() => void 0);
					}
				});
			} catch (err) {
				this.logger?.error("Failed to await subscription notification", {
					mintUrl,
					kind,
					err
				});
				reject(err);
			}
		});
	}
};

//#endregion
//#region api/HistoryApi.ts
var HistoryApi = class {
	historyService;
	constructor(historyService) {
		this.historyService = historyService;
	}
	async getPaginatedHistory(offset = 0, limit = 25) {
		return this.historyService.getPaginatedHistory(offset, limit);
	}
};

//#endregion
//#region plugins/PluginHost.ts
var PluginHost = class {
	plugins = [];
	cleanups = [];
	services;
	initialized = false;
	readyPhase = false;
	use(plugin) {
		this.plugins.push(plugin);
		if (this.initialized && this.services) {
			this.runInit(plugin, this.services);
			if (this.readyPhase) this.runReady(plugin, this.services);
		}
	}
	async init(services) {
		this.services = services;
		this.initialized = true;
		for (const p of this.plugins) await this.runInit(p, services);
	}
	async ready() {
		if (!this.services) return;
		this.readyPhase = true;
		for (const p of this.plugins) await this.runReady(p, this.services);
	}
	async dispose() {
		const errors = [];
		for (const p of this.plugins) try {
			await p.onDispose?.();
		} catch (err) {
			console.error("Plugin dispose error", {
				plugin: p.name,
				err
			});
			errors.push(err);
		}
		while (this.cleanups.length) {
			const fn = this.cleanups.pop();
			try {
				await fn();
			} catch (err) {
				errors.push(err);
			}
		}
		if (errors.length > 0) console.error("One or more plugin dispose/cleanup handlers failed");
	}
	async runInit(plugin, services) {
		const ctx = this.createContext(plugin, services);
		try {
			const cleanup = await plugin.onInit?.(ctx);
			if (typeof cleanup === "function") this.cleanups.push(cleanup);
		} catch (err) {
			console.error("Plugin init error", {
				plugin: plugin.name,
				err
			});
		}
	}
	async runReady(plugin, services) {
		const ctx = this.createContext(plugin, services);
		try {
			const cleanup = await plugin.onReady?.(ctx);
			if (typeof cleanup === "function") this.cleanups.push(cleanup);
		} catch (err) {
			console.error("Plugin ready error", {
				plugin: plugin.name,
				err
			});
		}
	}
	createContext(plugin, services) {
		const required = plugin.required ?? [];
		const selected = {};
		for (const k of required) selected[k] = services[k];
		return { services: selected };
	}
};

//#endregion
//#region Manager.ts
/**
* Initializes and configures a new Coco Cashu manager instance
* @param config - Configuration options including repositories, seed, and optional features
* @returns A fully initialized Manager instance
*/
async function initializeCoco(config) {
	await config.repo.init();
	const coco = new Manager(config.repo, config.seedGetter, config.logger, config.webSocketFactory, config.plugins, config.watchers, config.processors);
	const mintQuoteWatcherConfig = config.watchers?.mintQuoteWatcher;
	if (!mintQuoteWatcherConfig?.disabled) await coco.enableMintQuoteWatcher(mintQuoteWatcherConfig);
	if (!(config.watchers?.proofStateWatcher)?.disabled) await coco.enableProofStateWatcher();
	const mintQuoteProcessorConfig = config.processors?.mintQuoteProcessor;
	if (!mintQuoteProcessorConfig?.disabled) {
		await coco.enableMintQuoteProcessor(mintQuoteProcessorConfig);
		await coco.quotes.requeuePaidMintQuotes();
	}
	return coco;
}
var Manager = class {
	mint;
	wallet;
	quotes;
	keyring;
	subscription;
	history;
	mintService;
	walletService;
	proofService;
	walletRestoreService;
	keyRingService;
	eventBus;
	logger;
	subscriptions;
	mintQuoteService;
	mintQuoteWatcher;
	mintQuoteProcessor;
	mintQuoteRepository;
	proofStateWatcher;
	meltQuoteService;
	historyService;
	seedService;
	counterService;
	transactionService;
	paymentRequestService;
	pluginHost = new PluginHost();
	subscriptionsPaused = false;
	originalWatcherConfig;
	originalProcessorConfig;
	constructor(repositories, seedGetter, logger, webSocketFactory, plugins, watchers, processors) {
		this.logger = logger ?? new NullLogger();
		this.eventBus = this.createEventBus();
		this.subscriptions = this.createSubscriptionManager(webSocketFactory);
		this.originalWatcherConfig = watchers;
		this.originalProcessorConfig = processors;
		if (plugins && plugins.length > 0) for (const p of plugins) this.pluginHost.use(p);
		const core = this.buildCoreServices(repositories, seedGetter);
		this.mintService = core.mintService;
		this.walletService = core.walletService;
		this.proofService = core.proofService;
		this.walletRestoreService = core.walletRestoreService;
		this.keyRingService = core.keyRingService;
		this.seedService = core.seedService;
		this.counterService = core.counterService;
		this.mintQuoteService = core.mintQuoteService;
		this.mintQuoteRepository = core.mintQuoteRepository;
		this.meltQuoteService = core.meltQuoteService;
		this.historyService = core.historyService;
		this.transactionService = core.transactionService;
		this.paymentRequestService = core.paymentRequestService;
		const apis = this.buildApis();
		this.mint = apis.mint;
		this.wallet = apis.wallet;
		this.quotes = apis.quotes;
		this.keyring = apis.keyring;
		this.subscription = apis.subscription;
		this.history = apis.history;
		this.eventBus.on("mint:untrusted", ({ mintUrl }) => {
			this.logger.info("Mint untrusted, closing subscriptions", { mintUrl });
			this.subscriptions.closeMint(mintUrl);
		});
		const services = {
			mintService: this.mintService,
			walletService: this.walletService,
			proofService: this.proofService,
			keyRingService: this.keyRingService,
			seedService: this.seedService,
			walletRestoreService: this.walletRestoreService,
			counterService: this.counterService,
			mintQuoteService: this.mintQuoteService,
			meltQuoteService: this.meltQuoteService,
			historyService: this.historyService,
			transactionService: this.transactionService,
			subscriptions: this.subscriptions,
			eventBus: this.eventBus,
			logger: this.logger
		};
		this.pluginHost.init(services).then(() => this.pluginHost.ready()).catch((err) => {
			this.logger.error("Plugin system initialization failed", err);
		});
	}
	on(event, handler) {
		return this.eventBus.on(event, handler);
	}
	once(event, handler) {
		return this.eventBus.once(event, handler);
	}
	use(plugin) {
		this.pluginHost.use(plugin);
	}
	async dispose() {
		await this.pluginHost.dispose();
	}
	off(event, handler) {
		return this.eventBus.off(event, handler);
	}
	async enableMintQuoteWatcher(options) {
		if (this.mintQuoteWatcher?.isRunning()) return;
		const watcherLogger = this.logger.child ? this.logger.child({ module: "MintQuoteWatcherService" }) : this.logger;
		this.mintQuoteWatcher = new MintQuoteWatcherService(this.mintQuoteRepository, this.subscriptions, this.mintService, this.mintQuoteService, this.eventBus, watcherLogger, { watchExistingPendingOnStart: options?.watchExistingPendingOnStart ?? true });
		await this.mintQuoteWatcher.start();
	}
	async disableMintQuoteWatcher() {
		if (!this.mintQuoteWatcher) return;
		await this.mintQuoteWatcher.stop();
		this.mintQuoteWatcher = void 0;
	}
	async enableMintQuoteProcessor(options) {
		if (this.mintQuoteProcessor?.isRunning()) return false;
		const processorLogger = this.logger.child ? this.logger.child({ module: "MintQuoteProcessor" }) : this.logger;
		this.mintQuoteProcessor = new MintQuoteProcessor(this.mintQuoteService, this.eventBus, processorLogger, options);
		await this.mintQuoteProcessor.start();
		return true;
	}
	async disableMintQuoteProcessor() {
		if (!this.mintQuoteProcessor) return;
		await this.mintQuoteProcessor.stop();
		this.mintQuoteProcessor = void 0;
	}
	async waitForMintQuoteProcessor() {
		if (!this.mintQuoteProcessor) return;
		await this.mintQuoteProcessor.waitForCompletion();
	}
	async enableProofStateWatcher() {
		if (this.proofStateWatcher?.isRunning()) return;
		const watcherLogger = this.logger.child ? this.logger.child({ module: "ProofStateWatcherService" }) : this.logger;
		this.proofStateWatcher = new ProofStateWatcherService(this.subscriptions, this.mintService, this.proofService, this.eventBus, watcherLogger);
		await this.proofStateWatcher.start();
	}
	async disableProofStateWatcher() {
		if (!this.proofStateWatcher) return;
		await this.proofStateWatcher.stop();
		this.proofStateWatcher = void 0;
	}
	async pauseSubscriptions() {
		if (this.subscriptionsPaused) {
			this.logger.debug("Subscriptions already paused");
			return;
		}
		this.subscriptionsPaused = true;
		this.logger.info("Pausing subscriptions");
		this.subscriptions.pause();
		await this.disableMintQuoteWatcher();
		await this.disableProofStateWatcher();
		await this.disableMintQuoteProcessor();
		this.logger.info("Subscriptions paused");
	}
	async resumeSubscriptions() {
		this.subscriptionsPaused = false;
		this.logger.info("Resuming subscriptions");
		this.subscriptions.resume();
		const mintQuoteWatcherConfig = this.originalWatcherConfig?.mintQuoteWatcher;
		if (!mintQuoteWatcherConfig?.disabled) await this.enableMintQuoteWatcher(mintQuoteWatcherConfig);
		if (!(this.originalWatcherConfig?.proofStateWatcher)?.disabled) await this.enableProofStateWatcher();
		const mintQuoteProcessorConfig = this.originalProcessorConfig?.mintQuoteProcessor;
		if (!mintQuoteProcessorConfig?.disabled) {
			if (await this.enableMintQuoteProcessor(mintQuoteProcessorConfig)) await this.quotes.requeuePaidMintQuotes();
		}
		this.logger.info("Subscriptions resumed");
	}
	getChildLogger(moduleName) {
		return this.logger.child ? this.logger.child({ module: moduleName }) : this.logger;
	}
	createEventBus() {
		const eventLogger = this.getChildLogger("EventBus");
		return new EventBus({ onError: (args) => {
			eventLogger.error("Event handler error", args);
		} });
	}
	createSubscriptionManager(webSocketFactory) {
		const wsLogger = this.getChildLogger("SubscriptionManager");
		const defaultFactory = typeof globalThis.WebSocket !== "undefined" ? (url) => new globalThis.WebSocket(url) : void 0;
		const wsFactoryToUse = webSocketFactory ?? defaultFactory;
		const capabilitiesProvider = { getMintInfo: async (mintUrl) => {
			if (!this.mintService) throw new Error("MintService not initialized yet");
			return this.mintService.getMintInfo(mintUrl);
		} };
		if (!wsFactoryToUse) {
			const polling = new PollingTransport({ intervalMs: 5e3 }, wsLogger);
			return new SubscriptionManager(polling, wsLogger, capabilitiesProvider);
		}
		return new SubscriptionManager(wsFactoryToUse, wsLogger, capabilitiesProvider);
	}
	buildCoreServices(repositories, seedGetter) {
		const mintLogger = this.getChildLogger("MintService");
		const walletLogger = this.getChildLogger("WalletService");
		const counterLogger = this.getChildLogger("CounterService");
		const proofLogger = this.getChildLogger("ProofService");
		const mintQuoteLogger = this.getChildLogger("MintQuoteService");
		const walletRestoreLogger = this.getChildLogger("WalletRestoreService");
		const keyRingLogger = this.getChildLogger("KeyRingService");
		const meltQuoteLogger = this.getChildLogger("MeltQuoteService");
		const historyLogger = this.getChildLogger("HistoryService");
		const mintService = new MintService(repositories.mintRepository, repositories.keysetRepository, mintLogger, this.eventBus);
		const seedService = new SeedService(seedGetter);
		const keyRingService = new KeyRingService(repositories.keyRingRepository, seedService, keyRingLogger);
		const walletService = new WalletService(mintService, seedService, walletLogger);
		const counterService = new CounterService(repositories.counterRepository, counterLogger, this.eventBus);
		const proofService = new ProofService(counterService, repositories.proofRepository, walletService, keyRingService, seedService, proofLogger, this.eventBus);
		const walletRestoreService = new WalletRestoreService(proofService, counterService, walletService, walletRestoreLogger);
		const mintQuoteService = new MintQuoteService(repositories.mintQuoteRepository, mintService, walletService, proofService, this.eventBus, mintQuoteLogger);
		const mintQuoteRepository = repositories.mintQuoteRepository;
		const meltQuoteService = new MeltQuoteService(mintService, proofService, walletService, repositories.meltQuoteRepository, this.eventBus, meltQuoteLogger);
		const historyService = new HistoryService(repositories.historyRepository, this.eventBus, historyLogger);
		const transactionLogger = this.getChildLogger("TransactionService");
		const transactionService = new TransactionService(mintService, walletService, proofService, this.eventBus, transactionLogger);
		const paymentRequestLogger = this.getChildLogger("PaymentRequestService");
		const paymentRequestService = new PaymentRequestService(transactionService, paymentRequestLogger);
		return {
			mintService,
			seedService,
			walletService,
			counterService,
			proofService,
			walletRestoreService,
			keyRingService,
			mintQuoteService,
			mintQuoteRepository,
			meltQuoteService,
			historyService,
			transactionService,
			paymentRequestService
		};
	}
	buildApis() {
		const walletApiLogger = this.getChildLogger("WalletApi");
		const subscriptionApiLogger = this.getChildLogger("SubscriptionApi");
		const mint = new MintApi(this.mintService);
		const wallet = new WalletApi(this.mintService, this.walletService, this.proofService, this.walletRestoreService, this.transactionService, this.paymentRequestService, walletApiLogger);
		const quotes = new QuotesApi(this.mintQuoteService, this.meltQuoteService);
		const keyring = new KeyRingApi(this.keyRingService);
		const subscription = new SubscriptionApi(this.subscriptions, subscriptionApiLogger);
		const history = new HistoryApi(this.historyService);
		return {
			mint,
			wallet,
			quotes,
			keyring,
			subscription,
			history
		};
	}
};

//#endregion
//#region repositories/memory/MemoryCounterRepository.ts
var MemoryCounterRepository = class {
	counters = /* @__PURE__ */ new Map();
	key(mintUrl, keysetId) {
		return `${mintUrl}::${keysetId}`;
	}
	async getCounter(mintUrl, keysetId) {
		return this.counters.get(this.key(mintUrl, keysetId)) ?? null;
	}
	async setCounter(mintUrl, keysetId, counter) {
		const key = this.key(mintUrl, keysetId);
		this.counters.set(key, {
			mintUrl,
			keysetId,
			counter
		});
	}
};

//#endregion
//#region repositories/memory/MemoryKeysetRepository.ts
var MemoryKeysetRepository = class {
	keysetsByMint = /* @__PURE__ */ new Map();
	getMintMap(mintUrl) {
		if (!this.keysetsByMint.has(mintUrl)) this.keysetsByMint.set(mintUrl, /* @__PURE__ */ new Map());
		return this.keysetsByMint.get(mintUrl);
	}
	async getKeysetsByMintUrl(mintUrl) {
		return Array.from(this.getMintMap(mintUrl).values());
	}
	async getKeysetById(mintUrl, id) {
		return this.getMintMap(mintUrl).get(id) ?? null;
	}
	async updateKeyset(keyset) {
		const mintMap = this.getMintMap(keyset.mintUrl);
		const existing = mintMap.get(keyset.id);
		if (!existing) {
			mintMap.set(keyset.id, {
				...keyset,
				keypairs: {},
				updatedAt: Math.floor(Date.now() / 1e3)
			});
			return;
		}
		mintMap.set(keyset.id, {
			...existing,
			unit: keyset.unit,
			active: keyset.active,
			feePpk: keyset.feePpk,
			updatedAt: Math.floor(Date.now() / 1e3)
		});
	}
	async addKeyset(keyset) {
		this.getMintMap(keyset.mintUrl).set(keyset.id, {
			...keyset,
			updatedAt: Math.floor(Date.now() / 1e3)
		});
	}
	async deleteKeyset(mintUrl, keysetId) {
		this.getMintMap(mintUrl).delete(keysetId);
	}
};

//#endregion
//#region repositories/memory/MemoryKeyRingRepository.ts
var MemoryKeyRingRepository = class {
	keyPairs = /* @__PURE__ */ new Map();
	insertionOrder = [];
	async getPersistedKeyPair(publicKey) {
		return this.keyPairs.get(publicKey) ?? null;
	}
	async setPersistedKeyPair(keyPair) {
		if (!this.keyPairs.has(keyPair.publicKeyHex)) this.insertionOrder.push(keyPair.publicKeyHex);
		this.keyPairs.set(keyPair.publicKeyHex, keyPair);
	}
	async deletePersistedKeyPair(publicKey) {
		this.keyPairs.delete(publicKey);
		const index = this.insertionOrder.indexOf(publicKey);
		if (index !== -1) this.insertionOrder.splice(index, 1);
	}
	async getAllPersistedKeyPairs() {
		return Array.from(this.keyPairs.values());
	}
	async getLatestKeyPair() {
		if (this.insertionOrder.length === 0) return null;
		const latestPublicKey = this.insertionOrder[this.insertionOrder.length - 1];
		return this.keyPairs.get(latestPublicKey) ?? null;
	}
	async getLastDerivationIndex() {
		let maxIndex = -1;
		for (const keypair of this.keyPairs.values()) if (keypair.derivationIndex != null && keypair.derivationIndex > maxIndex) maxIndex = keypair.derivationIndex;
		return maxIndex;
	}
};

//#endregion
//#region repositories/memory/MemoryMintRepository.ts
var MemoryMintRepository = class {
	mints = /* @__PURE__ */ new Map();
	async isTrustedMint(mintUrl) {
		return this.mints.get(mintUrl)?.trusted ?? false;
	}
	async getMintByUrl(mintUrl) {
		const mint = this.mints.get(mintUrl);
		if (!mint) throw new Error(`Mint not found: ${mintUrl}`);
		return mint;
	}
	async getAllMints() {
		return Array.from(this.mints.values());
	}
	async getAllTrustedMints() {
		return Array.from(this.mints.values()).filter((mint) => mint.trusted);
	}
	async addNewMint(mint) {
		this.mints.set(mint.mintUrl, mint);
	}
	async addOrUpdateMint(mint) {
		this.mints.set(mint.mintUrl, mint);
	}
	async updateMint(mint) {
		this.mints.set(mint.mintUrl, mint);
	}
	async setMintTrusted(mintUrl, trusted) {
		const mint = this.mints.get(mintUrl);
		if (mint) {
			mint.trusted = trusted;
			this.mints.set(mintUrl, mint);
		}
	}
	async deleteMint(mintUrl) {
		this.mints.delete(mintUrl);
	}
};

//#endregion
//#region repositories/memory/MemoryProofRepository.ts
var MemoryProofRepository = class {
	proofsByMint = /* @__PURE__ */ new Map();
	getMintMap(mintUrl) {
		if (!this.proofsByMint.has(mintUrl)) this.proofsByMint.set(mintUrl, /* @__PURE__ */ new Map());
		return this.proofsByMint.get(mintUrl);
	}
	async saveProofs(mintUrl, proofs) {
		if (!proofs || proofs.length === 0) return;
		const map = this.getMintMap(mintUrl);
		for (const p of proofs) if (map.has(p.secret)) throw new Error(`Proof with secret already exists: ${p.secret}`);
		for (const p of proofs) map.set(p.secret, {
			...p,
			mintUrl
		});
	}
	async getReadyProofs(mintUrl) {
		const map = this.getMintMap(mintUrl);
		return Array.from(map.values()).filter((p) => p.state === "ready").map((p) => p);
	}
	async getAllReadyProofs() {
		const all = [];
		for (const map of this.proofsByMint.values()) for (const p of map.values()) if (p.state === "ready") all.push(p);
		return all;
	}
	async getProofsByKeysetId(mintUrl, keysetId) {
		const map = this.getMintMap(mintUrl);
		const results = [];
		for (const p of map.values()) if (p.state === "ready" && p.id === keysetId) results.push(p);
		return results;
	}
	async setProofState(mintUrl, secrets, state) {
		const map = this.getMintMap(mintUrl);
		for (const secret of secrets) {
			const p = map.get(secret);
			if (p) map.set(secret, {
				...p,
				state
			});
		}
	}
	async deleteProofs(mintUrl, secrets) {
		const map = this.getMintMap(mintUrl);
		for (const s of secrets) map.delete(s);
	}
	async wipeProofsByKeysetId(mintUrl, keysetId) {
		const map = this.getMintMap(mintUrl);
		for (const [secret, p] of Array.from(map.entries())) if (p.id === keysetId) map.delete(secret);
	}
};

//#endregion
//#region repositories/memory/MemoryMintQuoteRepository.ts
var MemoryMintQuoteRepository = class {
	quotes = /* @__PURE__ */ new Map();
	makeKey(mintUrl, quoteId) {
		return `${mintUrl}::${quoteId}`;
	}
	async getMintQuote(mintUrl, quoteId) {
		const key = this.makeKey(mintUrl, quoteId);
		return this.quotes.get(key) ?? null;
	}
	async addMintQuote(quote) {
		const key = this.makeKey(quote.mintUrl, quote.quote);
		this.quotes.set(key, quote);
	}
	async setMintQuoteState(mintUrl, quoteId, state) {
		const key = this.makeKey(mintUrl, quoteId);
		const existing = this.quotes.get(key);
		if (!existing) return;
		this.quotes.set(key, {
			...existing,
			state
		});
	}
	async getPendingMintQuotes() {
		const result = [];
		for (const q of this.quotes.values()) if (q.state !== "ISSUED") result.push(q);
		return result;
	}
};

//#endregion
//#region repositories/memory/MemoryMeltQuoteRepository.ts
var MemoryMeltQuoteRepository = class {
	quotes = /* @__PURE__ */ new Map();
	makeKey(mintUrl, quoteId) {
		return `${mintUrl}::${quoteId}`;
	}
	async getMeltQuote(mintUrl, quoteId) {
		const key = this.makeKey(mintUrl, quoteId);
		return this.quotes.get(key) ?? null;
	}
	async addMeltQuote(quote) {
		const key = this.makeKey(quote.mintUrl, quote.quote);
		this.quotes.set(key, quote);
	}
	async setMeltQuoteState(mintUrl, quoteId, state) {
		const key = this.makeKey(mintUrl, quoteId);
		const existing = this.quotes.get(key);
		if (!existing) return;
		this.quotes.set(key, {
			...existing,
			state
		});
	}
	async getPendingMeltQuotes() {
		const result = [];
		for (const q of this.quotes.values()) if (q.state !== "PAID") result.push(q);
		return result;
	}
};

//#endregion
//#region repositories/memory/MemoryHistoryRepository.ts
var MemoryHistoryRepository = class {
	entries = [];
	nextId = 1;
	async getPaginatedHistoryEntries(limit, offset) {
		return [...this.entries].sort((a, b) => {
			if (a.createdAt !== b.createdAt) return b.createdAt - a.createdAt;
			return Number(b.id) - Number(a.id);
		}).slice(offset, offset + limit);
	}
	async addHistoryEntry(history) {
		const entry = {
			id: String(this.nextId++),
			...history
		};
		this.entries.push(entry);
		return entry;
	}
	async getMintHistoryEntry(mintUrl, quoteId) {
		for (let i = this.entries.length - 1; i >= 0; i--) {
			const e = this.entries[i];
			if (!e) continue;
			if (e.type === "mint" && e.mintUrl === mintUrl && e.quoteId === quoteId) return e;
		}
		return null;
	}
	async getMeltHistoryEntry(mintUrl, quoteId) {
		for (let i = this.entries.length - 1; i >= 0; i--) {
			const e = this.entries[i];
			if (!e) continue;
			if (e.type === "melt" && e.mintUrl === mintUrl && e.quoteId === quoteId) return e;
		}
		return null;
	}
	async updateHistoryEntry(history) {
		const idx = this.entries.findIndex((e) => {
			if ((e.type === "mint" || e.type === "melt") && e.type === history.type) return e.mintUrl === history.mintUrl && e.quoteId === history.quoteId;
			return false;
		});
		if (idx === -1) throw new Error("History entry not found");
		const updated = {
			...this.entries[idx],
			...history
		};
		this.entries[idx] = updated;
		return updated;
	}
	async deleteHistoryEntry(mintUrl, quoteId) {
		for (let i = this.entries.length - 1; i >= 0; i--) {
			const e = this.entries[i];
			if (!e) continue;
			if ((e.type === "mint" || e.type === "melt") && e.mintUrl === mintUrl && e.quoteId === quoteId) this.entries.splice(i, 1);
		}
	}
};

//#endregion
//#region repositories/memory/MemoryRepositories.ts
var MemoryRepositories = class {
	mintRepository;
	keyRingRepository;
	counterRepository;
	keysetRepository;
	proofRepository;
	mintQuoteRepository;
	meltQuoteRepository;
	historyRepository;
	constructor() {
		this.mintRepository = new MemoryMintRepository();
		this.keyRingRepository = new MemoryKeyRingRepository();
		this.counterRepository = new MemoryCounterRepository();
		this.keysetRepository = new MemoryKeysetRepository();
		this.proofRepository = new MemoryProofRepository();
		this.mintQuoteRepository = new MemoryMintQuoteRepository();
		this.meltQuoteRepository = new MemoryMeltQuoteRepository();
		this.historyRepository = new MemoryHistoryRepository();
	}
	async init() {}
	async withTransaction(fn) {
		return fn(this);
	}
};

//#endregion
exports.ConsoleLogger = ConsoleLogger;
exports.CounterService = CounterService;
exports.HistoryApi = HistoryApi;
exports.HistoryService = HistoryService;
exports.HttpResponseError = HttpResponseError;
exports.KeyRingApi = KeyRingApi;
exports.KeyRingService = KeyRingService;
exports.KeysetSyncError = KeysetSyncError;
exports.Manager = Manager;
exports.MeltQuoteService = MeltQuoteService;
exports.MemoryCounterRepository = MemoryCounterRepository;
exports.MemoryHistoryRepository = MemoryHistoryRepository;
exports.MemoryKeyRingRepository = MemoryKeyRingRepository;
exports.MemoryKeysetRepository = MemoryKeysetRepository;
exports.MemoryMeltQuoteRepository = MemoryMeltQuoteRepository;
exports.MemoryMintQuoteRepository = MemoryMintQuoteRepository;
exports.MemoryMintRepository = MemoryMintRepository;
exports.MemoryProofRepository = MemoryProofRepository;
exports.MemoryRepositories = MemoryRepositories;
exports.MintApi = MintApi;
exports.MintFetchError = MintFetchError;
exports.MintOperationError = MintOperationError;
exports.MintQuoteProcessor = MintQuoteProcessor;
exports.MintQuoteService = MintQuoteService;
exports.MintQuoteWatcherService = MintQuoteWatcherService;
exports.MintService = MintService;
exports.NetworkError = NetworkError;
exports.PaymentRequestError = PaymentRequestError;
exports.PaymentRequestService = PaymentRequestService;
exports.PluginHost = PluginHost;
exports.ProofOperationError = ProofOperationError;
exports.ProofService = ProofService;
exports.ProofStateWatcherService = ProofStateWatcherService;
exports.ProofValidationError = ProofValidationError;
exports.QuotesApi = QuotesApi;
exports.SeedService = SeedService;
exports.SubscriptionApi = SubscriptionApi;
exports.SubscriptionManager = SubscriptionManager;
exports.TransactionService = TransactionService;
exports.UnknownMintError = UnknownMintError;
exports.WalletApi = WalletApi;
exports.WalletRestoreService = WalletRestoreService;
exports.WalletService = WalletService;
exports.WsConnectionManager = WsConnectionManager;
Object.defineProperty(exports, 'getDecodedToken', {
  enumerable: true,
  get: function () {
    return __cashu_cashu_ts.getDecodedToken;
  }
});
Object.defineProperty(exports, 'getEncodedToken', {
  enumerable: true,
  get: function () {
    return __cashu_cashu_ts.getEncodedToken;
  }
});
exports.initializeCoco = initializeCoco;
exports.normalizeMintUrl = normalizeMintUrl;